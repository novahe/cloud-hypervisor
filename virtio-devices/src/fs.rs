// Copyright © 2024 Intel Corporation
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::io;
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, Barrier, Mutex, result};

use anyhow::anyhow;
use fuse_backend_rs::api::server::Server;
use fuse_backend_rs::api::{
    Context, CreateIn, DirEntry, Entry, FileSystem, Handle, OpenOptions, Vfs, VfsOptions,
    ZeroCopyReader, ZeroCopyWriter,
};
use fuse_backend_rs::passthrough::{
    CachePolicy as BackendCachePolicy, Config as PassthroughConfig, PassthroughFs,
};
use fuse_backend_rs::transport::{Reader, VirtioFsWriter, Writer};
use log::{error, info};
use seccompiler::SeccompAction;
use serde::{Deserialize, Serialize};
use virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;
use virtio_queue::{Queue, QueueOwnedT};
use vm_memory::{ByteValued, GuestMemoryAtomic};
use vm_migration::{Migratable, MigratableError, Pausable, Snapshot, Snapshottable, Transportable};
use vmm_sys_util::eventfd::EventFd;

use super::{
    ActivateError, ActivateResult, EPOLL_HELPER_EVENT_LAST, EpollHelper, EpollHelperError,
    EpollHelperHandler, Error as DeviceError, VirtioCommon, VirtioDevice, VirtioDeviceType,
    VirtioInterrupt, VirtioInterruptType, VirtioSharedMemoryList,
};
use crate::GuestMemoryMmap;
use crate::seccomp_filters::Thread;
use crate::thread_helper::spawn_virtio_thread;

const NUM_QUEUE_OFFSET: usize = 1;
const QUEUE_AVAIL_EVENT: u16 = EPOLL_HELPER_EVENT_LAST + 1;

pub const VIRTIO_FS_TAG_LEN: usize = 36;

#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize)]
#[repr(C, packed)]
pub struct VirtioFsConfig {
    pub tag: [u8; VIRTIO_FS_TAG_LEN],
    pub num_request_queues: u32,
}

unsafe impl ByteValued for VirtioFsConfig {}

/// Minimum backend state for snapshot/restore.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct BackendState {
    /// Mapping of nodeid to its (parent_nodeid, name) for path resolution.
    pub node_mappings: BTreeMap<u64, (u64, PathBuf)>,
    /// active file/directory handles and their properties.
    pub handles: BTreeMap<u64, HandleState>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct HandleState {
    pub nodeid: u64,
    pub flags: u32,
    pub is_dir: bool,
}

/// A wrapper around FileSystem that tracks nodeids and handles for snapshotting.
struct TrackingFileSystem<F: FileSystem + Send + Sync> {
    inner: F,
    state: Arc<Mutex<BackendState>>,
    /// Maps Guest Inode ID -> Host Inode ID
    guest_to_host_inodes: Arc<Mutex<BTreeMap<u64, u64>>>,
    /// Maps Host Inode ID -> Guest Inode ID
    host_to_guest_inodes: Arc<Mutex<BTreeMap<u64, u64>>>,
    /// Maps Guest FH -> Host FH
    guest_to_host_handles: Arc<Mutex<BTreeMap<u64, u64>>>,
    /// Maps Host FH -> Guest FH
    host_to_guest_handles: Arc<Mutex<BTreeMap<u64, u64>>>,
}

impl<F: FileSystem + Send + Sync> TrackingFileSystem<F> {
    fn new(inner: F, state: Arc<Mutex<BackendState>>) -> Self {
        let mut guest_to_host_inodes = BTreeMap::new();
        let mut host_to_guest_inodes = BTreeMap::new();

        // FUSE root inode is always 1.
        guest_to_host_inodes.insert(1, 1);
        host_to_guest_inodes.insert(1, 1);

        TrackingFileSystem {
            inner,
            state,
            guest_to_host_inodes: Arc::new(Mutex::new(guest_to_host_inodes)),
            host_to_guest_inodes: Arc::new(Mutex::new(host_to_guest_inodes)),
            guest_to_host_handles: Arc::new(Mutex::new(BTreeMap::new())),
            host_to_guest_handles: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn path_component(name: &CStr) -> PathBuf {
        PathBuf::from(std::ffi::OsStr::from_bytes(name.to_bytes()))
    }

    fn map_guest_inode(&self, guest_inode: u64) -> io::Result<u64> {
        self.guest_to_host_inodes
            .lock()
            .unwrap()
            .get(&guest_inode)
            .cloned()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
    }

    fn map_host_inode(&self, host_inode: u64, guest_inode: u64) {
        let mut g2h = self.guest_to_host_inodes.lock().unwrap();
        let mut h2g = self.host_to_guest_inodes.lock().unwrap();
        g2h.insert(guest_inode, host_inode);
        h2g.insert(host_inode, guest_inode);
    }

    fn map_guest_handle(&self, guest_handle: u64) -> io::Result<u64> {
        self.guest_to_host_handles
            .lock()
            .unwrap()
            .get(&guest_handle)
            .cloned()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EBADF))
    }

    fn map_host_handle(&self, host_handle: u64, guest_handle: u64) {
        let mut g2h = self.guest_to_host_handles.lock().unwrap();
        let mut h2g = self.host_to_guest_handles.lock().unwrap();
        g2h.insert(guest_handle, host_handle);
        h2g.insert(host_handle, guest_handle);
    }

    fn remove_handle_mapping(&self, guest_handle: u64) {
        let mut g2h = self.guest_to_host_handles.lock().unwrap();
        let mut h2g = self.host_to_guest_handles.lock().unwrap();
        if let Some(h) = g2h.remove(&guest_handle) {
            h2g.remove(&h);
        }
    }

    fn insert_node_mapping(&self, nodeid: u64, parent: u64, name: &CStr) {
        let mut state = self.state.lock().unwrap();
        state
            .node_mappings
            .insert(nodeid, (parent, Self::path_component(name)));
    }

    fn remove_node_mapping(&self, parent: u64, name: &CStr) {
        let name = Self::path_component(name);
        let mut state = self.state.lock().unwrap();

        // 1. Find the target node being removed
        let target_node = state
            .node_mappings
            .iter()
            .find(|(_, (p, n))| *p == parent && *n == name)
            .map(|(id, _)| *id);

        if let Some(id) = target_node {
            // 2. Identify all descendants recursively
            let mut to_remove = vec![id];
            let mut queue = vec![id];

            while let Some(pid) = queue.pop() {
                let children: Vec<u64> = state
                    .node_mappings
                    .iter()
                    .filter(|(_, (p, _))| *p == pid)
                    .map(|(child_id, _)| *child_id)
                    .collect();

                for child_id in children {
                    to_remove.push(child_id);
                    queue.push(child_id);
                }
            }

            // 3. Perform the removal
            for node_id in to_remove {
                state.node_mappings.remove(&node_id);
                // Also clean up ID translation maps
                self.guest_to_host_inodes.lock().unwrap().remove(&node_id);
                self.host_to_guest_inodes.lock().unwrap().remove(&node_id);
            }
        }
    }

    fn update_node_mapping(
        &self,
        old_parent: u64,
        old_name: &CStr,
        new_parent: u64,
        new_name: &CStr,
    ) {
        let old_name = Self::path_component(old_name);
        let new_name = Self::path_component(new_name);
        let mut state = self.state.lock().unwrap();
        for (_, (mapped_parent, mapped_name)) in state.node_mappings.iter_mut() {
            if *mapped_parent == old_parent && *mapped_name == old_name {
                *mapped_parent = new_parent;
                *mapped_name = new_name.clone();
            }
        }
    }

    fn restore_state(&self) -> io::Result<()> {
        let state = self.state.lock().unwrap().clone();
        let ctx = Context::default();

        // 1. Replay Inode mappings
        // We need to replay lookups to establish inodes in the fresh PassthroughFs.
        // Since parents must exist before children, we iterate until all are resolved or we stop making progress.
        let mut resolved_nodes = Vec::new();
        resolved_nodes.push(1u64); // Root is always resolved

        let mut pending_mappings = state.node_mappings.clone();
        pending_mappings.remove(&1); // Skip root

        loop {
            let mut resolved_this_round = Vec::new();
            for (guest_node, (guest_parent, name)) in pending_mappings.iter() {
                if resolved_nodes.contains(guest_parent) {
                    let host_parent = self.map_guest_inode(*guest_parent)?;
                    let name_cstr = std::ffi::CString::new(name.as_os_str().as_bytes()).unwrap();
                    match self.inner.lookup(&ctx, host_parent.into(), &name_cstr) {
                        Ok(entry) => {
                            let host_node = entry.inode;
                            self.map_host_inode(host_node.into(), *guest_node);
                            resolved_this_round.push(*guest_node);
                        }
                        Err(e) => {
                            error!("Failed to restore node mapping for guest node {}: {}", guest_node, e);
                            return Err(e);
                        }
                    }
                }
            }

            if resolved_this_round.is_empty() {
                if !pending_mappings.is_empty() {
                    error!("Circular or dangling node mappings in restore state");
                    return Err(io::Error::from_raw_os_error(libc::EINVAL));
                }
                break;
            }

            for node in resolved_this_round {
                resolved_nodes.push(node);
                pending_mappings.remove(&node);
            }
        }

        // 2. Replay Handles
        for (guest_fh, handle_state) in state.handles.iter() {
            let host_node = self.map_guest_inode(handle_state.nodeid)?;
            let res = if handle_state.is_dir {
                self.inner.opendir(&ctx, host_node.into(), handle_state.flags)
                    .map(|(h, _)| h)
            } else {
                self.inner.open(&ctx, host_node.into(), handle_state.flags, 0)
                    .map(|(h, _, _)| h)
            };

            match res {
                Ok(Some(host_handle)) => {
                    self.map_host_handle(host_handle.into(), *guest_fh);
                }
                Ok(None) => {
                    error!("Backend returned no handle during restore for guest fh {}", guest_fh);
                    return Err(io::Error::from_raw_os_error(libc::EIO));
                }
                Err(e) => {
                    error!("Failed to restore handle for guest fh {}: {}", guest_fh, e);
                    return Err(e);
                }
            }
        }

        Ok(())
    }
}

impl<F: FileSystem + Send + Sync> FileSystem for TrackingFileSystem<F> {
    type Inode = F::Inode;
    type Handle = F::Handle;

    fn lookup(&self, ctx: &Context, parent: Self::Inode, name: &CStr) -> io::Result<Entry> {
        let host_parent = self.map_guest_inode(parent.into())?;
        let mut entry = self.inner.lookup(ctx, host_parent.into(), name)?;
        let host_inode = entry.inode;
        
        // Return existing guest inode if already mapped, otherwise record new mapping.
        // For lookup, guest handles assigning inodes? No, backend assigns.
        // Actually, during restore, we want to map MUST map host_inode -> guest_inode.
        // For new lookups, we just use the host_inode as guest_inode.
        let mut h2g = self.host_to_guest_inodes.lock().unwrap();
        let guest_inode = if let Some(g) = h2g.get(&host_inode) {
            *g
        } else {
            drop(h2g);
            self.map_host_inode(host_inode, host_inode);
            host_inode
        };
        
        entry.inode = guest_inode;
        self.insert_node_mapping(guest_inode, parent.into(), name);
        Ok(entry)
    }

    fn forget(&self, ctx: &Context, inode: Self::Inode, count: u64) {
        if let Ok(host_inode) = self.map_guest_inode(inode.into()) {
            self.inner.forget(ctx, host_inode.into(), count);
        }
    }

    fn open(
        &self,
        ctx: &Context,
        inode: Self::Inode,
        flags: u32,
        fuse_flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions, Option<u32>)> {
        let host_inode = self.map_guest_inode(inode.into())?;
        let (handle, opts, passthrough_open) = self.inner.open(ctx, host_inode.into(), flags, fuse_flags)?;
        
        if let Some(h) = handle {
            let host_fh = h.into();
            let guest_fh = host_fh;
            self.map_host_handle(host_fh, guest_fh);
            let mut state = self.state.lock().unwrap();
            state.handles.insert(
                guest_fh,
                HandleState {
                    nodeid: inode.into(),
                    flags,
                    is_dir: false,
                },
            );
        }
        Ok((handle, opts, passthrough_open))
    }

    fn opendir(
        &self,
        ctx: &Context,
        inode: Self::Inode,
        flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions)> {
        let host_inode = self.map_guest_inode(inode.into())?;
        let (handle, opts) = self.inner.opendir(ctx, host_inode.into(), flags)?;
        if let Some(h) = handle {
            let host_fh = h.into();
            let guest_fh = host_fh;
            self.map_host_handle(host_fh, guest_fh);
            let mut state = self.state.lock().unwrap();
            state.handles.insert(
                guest_fh,
                HandleState {
                    nodeid: inode.into(),
                    flags,
                    is_dir: true,
                },
            );
        }
        Ok((handle, opts))
    }

    fn release(
        &self,
        ctx: &Context,
        inode: Self::Inode,
        flags: u32,
        handle: Self::Handle,
        flush: bool,
        flock: bool,
        lock_owner: Option<u64>,
    ) -> io::Result<()> {
        let guest_fh = handle.into();
        let host_fh = self.map_guest_handle(guest_fh)?;
        let host_inode = self.map_guest_inode(inode.into())?;
        
        self.inner
            .release(ctx, host_inode.into(), flags, host_fh.into(), flush, flock, lock_owner)?;
        
        self.remove_handle_mapping(guest_fh);
        let mut state = self.state.lock().unwrap();
        state.handles.remove(&guest_fh);
        Ok(())
    }

    fn releasedir(
        &self,
        ctx: &Context,
        inode: Self::Inode,
        flags: u32,
        handle: Self::Handle,
    ) -> io::Result<()> {
        let guest_fh = handle.into();
        let host_fh = self.map_guest_handle(guest_fh)?;
        let host_inode = self.map_guest_inode(inode.into())?;
        
        self.inner.releasedir(ctx, host_inode.into(), flags, host_fh.into())?;
        
        self.remove_handle_mapping(guest_fh);
        let mut state = self.state.lock().unwrap();
        state.handles.remove(&guest_fh);
        Ok(())
    }

    fn create(
        &self,
        ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
        args: CreateIn,
    ) -> io::Result<(Entry, Option<Self::Handle>, OpenOptions, Option<u32>)> {
        let host_parent = self.map_guest_inode(parent.into())?;
        let (mut entry, handle, opts, passthrough_open) = self.inner.create(ctx, host_parent.into(), name, args)?;
        
        let host_inode = entry.inode;
        self.map_host_inode(host_inode, host_inode);
        entry.inode = host_inode;
        
        self.insert_node_mapping(host_inode, parent.into(), name);
        if let Some(h) = handle {
            let host_fh = h.into();
            let guest_fh = host_fh;
            self.map_host_handle(host_fh, guest_fh);
            let mut state = self.state.lock().unwrap();
            state.handles.insert(
                guest_fh,
                HandleState {
                    nodeid: host_inode,
                    flags: args.flags,
                    is_dir: false,
                },
            );
        }
        Ok((entry, handle, opts, passthrough_open))
    }

    fn mkdir(
        &self,
        ctx: &Context,
        parent: Self::Inode,
        name: &CStr,
        mode: u32,
        umask: u32,
    ) -> io::Result<Entry> {
        let host_parent = self.map_guest_inode(parent.into())?;
        let mut entry = self.inner.mkdir(ctx, host_parent.into(), name, mode, umask)?;
        let host_inode = entry.inode;
        self.map_host_inode(host_inode, host_inode);
        entry.inode = host_inode;
        self.insert_node_mapping(host_inode, parent.into(), name);
        Ok(entry)
    }

    fn unlink(&self, ctx: &Context, parent: Self::Inode, name: &CStr) -> io::Result<()> {
        let host_parent = self.map_guest_inode(parent.into())?;
        self.inner.unlink(ctx, host_parent.into(), name)?;
        self.remove_node_mapping(parent.into(), name);
        Ok(())
    }

    fn rmdir(&self, ctx: &Context, parent: Self::Inode, name: &CStr) -> io::Result<()> {
        let host_parent = self.map_guest_inode(parent.into())?;
        self.inner.rmdir(ctx, host_parent.into(), name)?;
        self.remove_node_mapping(parent.into(), name);
        Ok(())
    }

    fn rename(
        &self,
        ctx: &Context,
        olddir: Self::Inode,
        oldname: &CStr,
        newdir: Self::Inode,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        let host_olddir = self.map_guest_inode(olddir.into())?;
        let host_newdir = self.map_guest_inode(newdir.into())?;
        self.inner
            .rename(ctx, host_olddir.into(), oldname, host_newdir.into(), newname, flags)?;
        self.update_node_mapping(olddir.into(), oldname, newdir.into(), newname);
        Ok(())
    }

    fn getattr(
        &self,
        ctx: &Context,
        inode: Self::Inode,
        handle: Option<Self::Handle>,
    ) -> io::Result<(libc::stat64, u64)> {
        let host_inode = self.map_guest_inode(inode.into())?;
        let host_handle = if let Some(h) = handle {
            Some(self.map_guest_handle(h.into())?.into())
        } else {
            None
        };
        self.inner.getattr(ctx, host_inode.into(), host_handle)
    }

    fn read(
        &self,
        ctx: &Context,
        inode: Self::Inode,
        handle: Self::Handle,
        w: &mut dyn ZeroCopyWriter,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        flags: u32,
    ) -> io::Result<usize> {
        let host_inode = self.map_guest_inode(inode.into())?;
        let host_handle = self.map_guest_handle(handle.into())?;
        self.inner
            .read(ctx, host_inode.into(), host_handle.into(), w, size, offset, lock_owner, flags)
    }

    fn write(
        &self,
        ctx: &Context,
        inode: Self::Inode,
        handle: Self::Handle,
        r: &mut dyn ZeroCopyReader,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        delayed_write: bool,
        flags: u32,
        fuse_flags: u32,
    ) -> io::Result<usize> {
        let host_inode = self.map_guest_inode(inode.into())?;
        let host_handle = self.map_guest_handle(handle.into())?;
        self.inner.write(
            ctx,
            host_inode.into(),
            host_handle.into(),
            r,
            size,
            offset,
            lock_owner,
            delayed_write,
            flags,
            fuse_flags,
        )
    }

    fn readdir(
        &self,
        ctx: &Context,
        inode: Self::Inode,
        handle: Self::Handle,
        size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let host_inode = self.map_guest_inode(inode.into())?;
        let host_handle = self.map_guest_handle(handle.into())?;
        self.inner
            .readdir(ctx, host_inode.into(), host_handle.into(), size, offset, add_entry)
    }
}

struct FsHandler {
    queues: Vec<Queue>,
    mem: GuestMemoryAtomic<GuestMemoryMmap>,
    server: Arc<Server<Arc<Vfs>>>,
    interrupt_cb: Arc<dyn VirtioInterrupt>,
    kill_evt: EventFd,
    pause_evt: EventFd,
    queue_evts: Vec<EventFd>,
}

impl FsHandler {
    fn process_queue(&mut self, queue_index: usize) -> result::Result<bool, DeviceError> {
        let mem = self.mem.memory();
        let queue = &mut self.queues[queue_index];
        let mut used_descs = false;
        let mut iter = queue
            .iter(mem.deref())
            .map_err(DeviceError::QueueIterator)?;

        for desc_chain in &mut iter {
            let head_index = desc_chain.head_index();

            let reader = Reader::from_descriptor_chain(&mem, desc_chain.clone()).map_err(|e| {
                error!("failed to create reader: {}", e);
                DeviceError::IoError(io::Error::new(io::ErrorKind::Other, e))
            })?;
            let writer = VirtioFsWriter::new(mem.deref(), desc_chain.clone()).map_err(|e| {
                error!("failed to create writer: {}", e);
                DeviceError::IoError(io::Error::new(io::ErrorKind::Other, e))
            })?;

            let bytes_written = self
                .server
                .handle_message(reader, Writer::VirtioFs(writer), None, None)
                .map_err(|e| {
                    error!("failed to handle fuse message: {}", e);
                    DeviceError::IoError(io::Error::new(io::ErrorKind::Other, e))
                })?;

            queue
                .add_used(mem.deref(), head_index, bytes_written as u32)
                .map_err(DeviceError::QueueAddUsed)?;
            used_descs = true;
        }

        if !used_descs {
            return Ok(false);
        }

        queue
            .needs_notification(mem.deref())
            .map_err(DeviceError::QueueIterator)
    }

    fn signal_used_queue(&self, queue_index: u16) -> result::Result<(), DeviceError> {
        self.interrupt_cb
            .trigger(VirtioInterruptType::Queue(queue_index))
            .map_err(|e| {
                error!("Failed to signal used queue: {:?}", e);
                DeviceError::FailedSignalingUsedQueue(e)
            })
    }
}

impl EpollHelperHandler for FsHandler {
    fn handle_event(
        &mut self,
        _helper: &mut EpollHelper,
        event: &vmm_sys_util::epoll::Event,
    ) -> result::Result<(), EpollHelperError> {
        let ev_type = event.data as u16;
        match ev_type {
            QUEUE_AVAIL_EVENT.. => {
                let queue_index = (ev_type - QUEUE_AVAIL_EVENT) as usize;
                let _ = self.queue_evts[queue_index].read();
                match self.process_queue(queue_index) {
                    Ok(used) => {
                        if used {
                            let _ = self.signal_used_queue(queue_index as u16);
                        }
                    }
                    Err(e) => {
                        error!("Failed to process queue {}: {:?}", queue_index, e);
                        return Err(EpollHelperError::HandleEvent(anyhow!(e)));
                    }
                }
            }
            _ => {
                return Err(EpollHelperError::HandleEvent(anyhow!("Unknown event type")));
            }
        }
        Ok(())
    }
}

pub struct Fs {
    common: VirtioCommon,
    id: String,
    server: Arc<Server<Arc<Vfs>>>,
    config: VirtioFsConfig,
    seccomp_action: SeccompAction,
    exit_evt: EventFd,
    shm_list: Option<VirtioSharedMemoryList>,
    backend_state: Arc<Mutex<BackendState>>,
}

#[derive(Serialize, Deserialize)]
pub struct FsState {
    pub avail_features: u64,
    pub acked_features: u64,
    pub config: VirtioFsConfig,
    pub queue_sizes: Vec<u16>,
    pub backend: BackendState,
}

impl Fs {
    pub fn new(
        id: String,
        tag: &str,
        shared_dir: PathBuf,
        req_num_queues: usize,
        queue_size: u16,
        cache_policy: &str,
        writeback: bool,
        seccomp_action: SeccompAction,
        exit_evt: EventFd,
        shm_list: Option<VirtioSharedMemoryList>,
        state: Option<FsState>,
    ) -> io::Result<Self> {
        let num_queues = NUM_QUEUE_OFFSET + req_num_queues;
        let mut fs_tag = [0u8; VIRTIO_FS_TAG_LEN];
        let tag_bytes = tag.as_bytes();
        let len = std::cmp::min(tag_bytes.len(), VIRTIO_FS_TAG_LEN);
        fs_tag[..len].copy_from_slice(&tag_bytes[..len]);

        let backend_cache_policy = cache_policy
            .parse::<BackendCachePolicy>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        let is_restore = state.is_some();
        let (avail_features, acked_features, config, queue_sizes, backend_state, paused) =
            if let Some(state) = state {
                info!("Restoring virtio-fs {}", id);
                (
                    state.avail_features,
                    state.acked_features,
                    state.config,
                    state.queue_sizes,
                    Arc::new(Mutex::new(state.backend)),
                    true,
                )
            } else {
                (
                    1 << VIRTIO_F_VERSION_1,
                    0,
                    VirtioFsConfig {
                        tag: fs_tag,
                        num_request_queues: req_num_queues as u32,
                    },
                    vec![queue_size; num_queues],
                    Arc::new(Mutex::new(BackendState::default())),
                    false,
                )
            };

        let vfs = Arc::new(Vfs::new(VfsOptions {
            no_writeback: !writeback,
            ..VfsOptions::default()
        }));
        let passthrough_cfg = PassthroughConfig {
            root_dir: shared_dir.to_str().unwrap().to_string(),
            cache_policy: backend_cache_policy,
            writeback,
            ..Default::default()
        };
        let inner_fs = PassthroughFs::new(passthrough_cfg)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let tracking_fs = TrackingFileSystem::new(inner_fs, backend_state.clone());
        if is_restore {
            tracking_fs.restore_state()?;
        }
        vfs.mount(Box::new(tracking_fs), "/")
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let server = Arc::new(Server::new(vfs));

        Ok(Fs {
            common: VirtioCommon {
                device_type: VirtioDeviceType::Fs as u32,
                avail_features,
                acked_features,
                paused_sync: Some(Arc::new(Barrier::new(2))),
                min_queues: 1,
                queue_sizes,
                paused: Arc::new(std::sync::atomic::AtomicBool::new(paused)),
                ..Default::default()
            },
            id,
            server,
            config,
            seccomp_action,
            exit_evt,
            shm_list,
            backend_state,
        })
    }

    fn state(&self) -> FsState {
        FsState {
            avail_features: self.common.avail_features,
            acked_features: self.common.acked_features,
            config: self.config,
            queue_sizes: self.common.queue_sizes.clone(),
            backend: self.backend_state.lock().unwrap().clone(),
        }
    }
}

impl VirtioDevice for Fs {
    fn device_type(&self) -> u32 {
        self.common.device_type
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.common.queue_sizes
    }

    fn features(&self) -> u64 {
        self.common.avail_features
    }

    fn ack_features(&mut self, value: u64) {
        self.common.ack_features(value)
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        self.read_config_from_slice(self.config.as_slice(), offset, data)
    }

    fn activate(&mut self, context: crate::device::ActivationContext) -> ActivateResult {
        let crate::device::ActivationContext {
            mem,
            interrupt_cb,
            mut queues,
            ..
        } = context;
        self.common.activate(&queues, interrupt_cb.clone())?;

        let (kill_evt, pause_evt) = self.common.dup_eventfds();
        let mut handler_queues = Vec::new();
        let mut queue_evts = Vec::new();
        for (_, q, q_evt) in queues.drain(..) {
            handler_queues.push(q);
            queue_evts.push(q_evt);
        }

        let mut handler = FsHandler {
            queues: handler_queues,
            mem,
            server: self.server.clone(),
            interrupt_cb,
            kill_evt,
            pause_evt,
            queue_evts,
        };

        let paused = self.common.paused.clone();
        let paused_sync = self.common.paused_sync.clone();
        let mut epoll_threads = Vec::new();

        spawn_virtio_thread(
            &format!("{}_worker", &self.id),
            &self.seccomp_action,
            Thread::VirtioFs,
            &mut epoll_threads,
            &self.exit_evt,
            move || {
                let mut epoll_helper = EpollHelper::new(&handler.kill_evt, &handler.pause_evt)?;
                for (i, queue_evt) in handler.queue_evts.iter().enumerate() {
                    epoll_helper.add_event(queue_evt.as_raw_fd(), QUEUE_AVAIL_EVENT + i as u16)?;
                }
                epoll_helper.run(&paused, paused_sync.as_ref().unwrap(), &mut handler)
            },
        )?;

        self.common.epoll_threads = Some(epoll_threads);
        Ok(())
    }

    fn reset(&mut self) -> Option<Arc<dyn VirtioInterrupt>> {
        self.common.reset()
    }

    fn get_shm_regions(&self) -> Option<VirtioSharedMemoryList> {
        self.shm_list.clone()
    }
}

impl Pausable for Fs {
    fn pause(&mut self) -> std::result::Result<(), MigratableError> {
        self.common.pause()
    }

    fn resume(&mut self) -> std::result::Result<(), MigratableError> {
        self.common.resume()
    }
}
impl Snapshottable for Fs {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn snapshot(&mut self) -> std::result::Result<Snapshot, MigratableError> {
        Snapshot::new_from_state(&self.state())
    }
}
impl Transportable for Fs {}
impl Migratable for Fs {
    fn set_paused(&mut self, paused: bool) -> std::result::Result<(), MigratableError> {
        if paused { self.pause() } else { self.resume() }
    }
}
