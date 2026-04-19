# Native Virtio-FS POC Plan

## Goal

Implement a built-in native virtio-fs device in Cloud Hypervisor using
`fuse-backend-rs`, while preserving the existing vhost-user virtio-fs path.

This POC must support:

- built-in virtio-fs mount and basic I/O
- DAX shared memory window
- snapshot and restore

This POC does not commit to live migration support.

## Non-Goals

- replacing the existing vhost-user virtio-fs backend
- full live migration support
- production-grade snapshot semantics for every filesystem mutation case

## Delivery Scope

The implementation will add a second virtio-fs backend:

- `vhost-user`: existing socket-based path, unchanged
- `builtin`: new in-process path backed by `fuse-backend-rs`

The snapshot/restore support for the built-in backend will be path-based and
explicitly experimental. Restore will fail fast if tracked paths cannot be
reopened safely.

## Key Constraints

- Cloud Hypervisor's current virtio device model does not expose Dragonball's
  `get_resource_requirements()` API. DAX must be integrated through
  `VirtioSharedMemoryList`, `userspace_mappings()`, and the existing
  virtio-pci shared-memory BAR flow.
- Existing snapshot/restore support for `vhost-user` virtio-fs must remain
  intact.
- Landlock must continue to constrain filesystem access.
- Seccomp filters must be updated for the new worker thread syscall surface.

## Architecture

### Configuration Model

Replace the current socket-only fs configuration with an explicit backend enum
in Rust, while keeping the external config surface backward compatible.

Target structure in `vmm/src/vm_config.rs`:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum FsBackendConfig {
    VhostUser { socket: PathBuf },
    Builtin {
        shared_dir: PathBuf,
        cache_size: u64,
        cache_policy: FsCachePolicy,
        writeback: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum FsCachePolicy {
    Always,
    Auto,
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct FsConfig {
    #[serde(flatten)]
    pub pci_common: PciDeviceCommonConfig,
    pub tag: String,
    pub backend: FsBackendConfig,
    #[serde(default = "default_fsconfig_num_queues")]
    pub num_queues: usize,
    #[serde(default = "default_fsconfig_queue_size")]
    pub queue_size: u16,
}
```

CLI examples:

```bash
--fs tag=myfs,socket=/tmp/virtiofs.sock
--fs tag=myfs,shared_dir=/srv/share,cache=always,writeback=on,cache_size=0
--fs tag=myfs,shared_dir=/srv/share,cache=always,writeback=on,cache_size=2147483648
```

Validation rules:

- exactly one of `socket` or `shared_dir` must be specified
- `cache_size > 0` implies DAX enabled
- builtin mode with `cache_size > 0` requires `memory.shared=on`
- `num_queues <= boot_vcpus`
- builtin mode requires an existing `shared_dir`

API compatibility note:

- the internal Rust model should use `FsBackendConfig`
- the CLI parser and API payload may remain flat with optional `socket` and
  `shared_dir` fields for backward compatibility
- parser validation will construct the enum and reject invalid combinations
- the OpenAPI schema should express mutual exclusion with `oneOf` if practical,
  while keeping the legacy `socket` field shape accepted

### Device Shape

Add a new native fs device implemented in `virtio-devices/src/fs/`.

Core components:

- `device.rs`: `Fs` device object implementing `VirtioDevice`
- `handler.rs`: queue processing loop using `EpollHelperHandler`
- `state.rs`: snapshot state serialization structures
- `mod.rs`: exports and module glue

The device will use:

- `fuse_backend_rs::passthrough::PassthroughFs`
- `fuse_backend_rs::api::Vfs`
- `fuse_backend_rs::api::server::Server`
- `fuse_backend_rs::transport::{Reader, VirtioFsWriter, Writer}`

Passthrough configuration mapping:

- `cache_policy` maps to `PassthroughConfig.cache_policy`
- `writeback` maps to `VfsOptions.no_writeback = !writeback`
- the initial POC should also carry through `no_open`, `killpriv_v2`, and
  `no_readdir` decisions explicitly, even if some are hardcoded defaults in the
  first revision
- timeout behavior must be derived from cache policy in the same spirit as the
  Dragonball implementation

### DAX Integration

Do not copy Dragonball's resource APIs directly. Instead:

1. `Fs::new()` creates the memfd-backed `MmapRegion` and an initial
   `VirtioSharedMemoryList` with a placeholder guest address.
2. `device_manager` allocates the guest physical range and KVM memslot for that
   mapping, reusing the existing userspace-mapping path.
3. `device_manager` updates the device via `set_shm_regions()` with the final
   guest base address.
4. The existing virtio-pci transport creates the shared-memory BAR and
   capabilities from `get_shm_regions()`.

This matches the current transport behavior in
`virtio-devices/src/transport/pci_device.rs`.

Important distinction:

- pmem and builtin virtio-fs can both rely on `device_manager` for guest
  address allocation and memslot registration
- the virtio-pci SHM BAR is still created by transport from the device's shared
  memory metadata, just like current virtio-fs and other SHM-capable devices

### Snapshot Model

Built-in virtio-fs snapshot/restore will use a path-based tracked state model
for the POC.

The tracker records:

- active FUSE node identifiers
- current resolved path
- open file and directory handles
- open flags
- object type

Restore behavior:

- rebuild the fs backend
- reopen tracked paths
- fail restore if any required tracked path cannot be reopened

This POC will not attempt transparent degradation for missing paths.

Node identifier source:

- use FUSE `nodeid` from lookup/entry results, not host `stat(2)` inode
- do not rely on host inode values as restore identity
- before committing to the wrapper design, validate that
  `fuse-backend-rs` exposes enough public information for a delegating wrapper
  to track the returned node identifiers correctly

## File-by-File Change Plan

### 1. `virtio-devices/Cargo.toml`

Add dependencies:

- `fuse-backend-rs`
- `threadpool`
- `caps`
- `nix`
- `rlimit`

Keep the dependency set minimal and avoid importing Dragonball-only
infrastructure.

Also add a feature gate:

- workspace feature: `native_virtiofs`
- `virtio-devices` feature: `native_virtiofs`

Default builds should remain unchanged unless this feature is enabled.

### 2. `virtio-devices/src/lib.rs`

Add:

```rust
pub mod fs;
pub use self::fs::{Fs, FsState};
```

### 3. `virtio-devices/src/fs/mod.rs`

Create the new module and re-export:

- `Fs`
- `FsState`
- tracking-related state types if needed by tests

### 4. `virtio-devices/src/fs/device.rs`

Implement the native device.

Planned structure:

```rust
pub struct Fs {
    common: VirtioCommon,
    id: String,
    tag: String,
    server: Arc<Server<Arc<Vfs>>>,
    passthrough: Arc<TrackingFs>,
    shm_regions: Option<VirtioSharedMemoryList>,
    guest_memory: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    snapshot_tracker: Arc<Mutex<SnapshotTracker>>,
    cache_policy: FsCachePolicyState,
    writeback: bool,
    cache_size: u64,
    seccomp_action: SeccompAction,
    exit_evt: EventFd,
}
```

Implement:

- `Fs::new(...)`
- `VirtioDevice`
  - `device_type()`
  - `queue_max_sizes()`
  - `features()`
  - `read_config()`
  - `activate()`
  - `get_shm_regions()`
  - `set_shm_regions()`
  - `userspace_mappings()`
- `Pausable`
- `Snapshottable`
- `Transportable`
- `Migratable`

Important implementation notes:

- build config space with tag and request queue count
- if `cache_size == 0`, do not expose shared memory regions
- thread startup must use `spawn_virtio_thread()`
- use CH's `EpollHelper` conventions rather than Dragonball's event manager
- `FsState` must include enough `VirtioCommon` state to restore negotiated
  device behavior, at minimum `avail_features` and `acked_features`
- `Fs::new(..., state: Option<FsState>)` restores both filesystem state and
  negotiated virtio feature state

### 5. `virtio-devices/src/fs/handler.rs`

Implement:

- `CacheHandler` for DAX map/unmap requests
- `FsEpollHandler` implementing `EpollHelperHandler`

Queue loop responsibilities:

- pull descriptor chains from virtio queue
- create `Reader`
- create `Writer::VirtioFs(VirtioFsWriter::new(...))`
- call `server.handle_message(...)`
- add used descriptors and notify queue

Execution model:

- start with a single event thread
- optionally use `threadpool` for request execution if needed for parity with
  Dragonball's queue handling
- keep the initial POC simple if correctness is easier without a thread pool

DAX mapping policy:

- start with read-only DAX mmap behavior
- in `CacheHandler::map()`, inspect mapping flags and reject writable mappings
  explicitly
- prefer a deterministic FUSE-side failure over allowing a writable mapping
  that later turns into guest-visible memory faults
- document that guest writable DAX mappings are out of scope for the POC

### 6. `virtio-devices/src/fs/state.rs`

Define serializable snapshot state:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FsState {
    pub tag: String,
    pub avail_features: u64,
    pub acked_features: u64,
    pub cache_policy: FsCachePolicyState,
    pub writeback: bool,
    pub cache_size: u64,
    pub open_handles: Vec<TrackedHandleState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackedHandleState {
    pub nodeid: u64,
    pub path: PathBuf,
    pub flags: i32,
    pub file_type: TrackedFileType,
}
```

Also define:

- `SnapshotTracker`
- `TrackingFs`

`TrackingFs` wraps `PassthroughFs` and intercepts operations needed to keep the
tracker current:

- `lookup`
- `open`
- `opendir`
- `release`
- `releasedir`
- `create`
- `mkdir`
- `rename`
- `unlink`
- `rmdir`

Compatibility spike required before full Step 4:

- write a minimal `TrackingFs` wrapper that implements the full
  `fuse-backend-rs` `FileSystem` trait by delegation
- prove that context, reply types, and handle types can be forwarded cleanly
- only after this spike passes should the full tracker implementation proceed

Implementation note:

- `FileSystem` has a large method surface, so `TrackingFs` must implement all
  methods and delegate most of them unchanged
- tracking hooks are only inserted in the limited mutation and handle lifecycle
  methods listed above

### 7. `virtio-devices/src/seccomp_filters.rs`

Add a new thread type:

```rust
Thread::VirtioFs
```

Add syscall allowlist for the native worker thread, at minimum:

- `openat`
- `close`
- `read`
- `write`
- `pread64`
- `pwrite64`
- `readv`
- `writev`
- `newfstatat`
- `statx`
- `getdents64`
- `readlinkat`
- `unlinkat`
- `renameat2`
- `mkdirat`
- `ftruncate`
- `fallocate`
- `fchmod`
- `fchmodat`
- `fchownat`
- `fsetxattr`
- `fgetxattr`
- `flistxattr`
- `fremovexattr`
- `mmap`
- `munmap`
- `mprotect`
- `epoll_*`
- `futex`

This list should be tightened after implementation and test runs.

### 8. `vmm/src/vm_config.rs`

Refactor `FsConfig` to use `FsBackendConfig`.

Update `ApplyLandlock`:

- `VhostUser`: existing socket `rw`
- `Builtin`: `shared_dir` `rw`

Landlock already uses `path_beneath_rules`, so granting a directory rule covers
its subtree.

### 9. `vmm/src/config.rs`

Extend parser and validation logic.

Parser changes:

- parse `socket`
- parse `shared_dir`
- parse `cache`
- parse `writeback`
- parse `cache_size`

Validation changes:

- reject invalid backend combinations
- reject missing backend
- enforce queue constraints
- enforce shared memory prerequisite for DAX

Tests to update and add:

- existing socket-based fs parsing remains valid
- builtin parsing works
- invalid combinations fail

### 10. `vmm/src/device_manager.rs`

Update `make_virtio_fs_device()`.

Behavior:

- `FsBackendConfig::VhostUser`: keep current flow
- `FsBackendConfig::Builtin`:
  - create DAX guest address and memslot if `cache_size > 0`
  - create native `virtio_devices::Fs`
  - register migratable device into device tree

For DAX mapping:

- let `Fs::new()` own memfd and `MmapRegion` creation
- use the same general userspace-mapping approach already used by pmem for
  guest address allocation and KVM memslot registration
- update the device with the final `VirtioSharedMemoryList` after address
  allocation

### 11. `vmm/src/api/openapi/cloud-hypervisor.yaml`

Update fs schema:

- keep legacy `socket` support visible
- add `shared_dir`
- add `cache`
- add `writeback`
- add `cache_size`
- use `oneOf` or equivalent schema expression for mutual exclusivity if the
  generator allows it without breaking existing consumers

### 12. `cloud-hypervisor/tests/integration.rs`

Add built-in virtio-fs integration coverage:

- `test_builtin_virtio_fs_basic`
- `test_builtin_virtio_fs_dax_disabled`
- `test_builtin_virtio_fs_dax_enabled`
- `test_snapshot_restore_builtin_virtio_fs`
- `test_snapshot_restore_builtin_virtio_fs_open_files`
- `test_snapshot_restore_builtin_virtio_fs_unlinked_file_fails`

### 13. Optional targeted unit tests

Add tests in:

- `virtio-devices/src/fs/device.rs`
- `virtio-devices/src/fs/state.rs`
- `vmm/src/config.rs`

Focus on:

- config encoding
- cache policy parsing
- DAX region exposure
- snapshot state roundtrip
- rename and unlink tracker updates
- delegating `TrackingFs` wrapper viability

## Implementation Order

### Step 1: Config and schema

Implement:

- `FsBackendConfig`
- parser updates
- validation
- Landlock updates
- API schema updates

Checkpoint:

- socket-based fs still parses and validates
- builtin fs parses and validates

### Step 2: Native device skeleton

Create:

- `virtio-devices/src/fs/mod.rs`
- `virtio-devices/src/fs/device.rs`
- `virtio-devices/src/fs/handler.rs`

Implement:

- config space
- queue activation
- request handling
- no-DAX basic I/O

Checkpoint:

- guest can mount builtin virtio-fs and perform read/write

### Step 3: FileSystem wrapper spike

Implement:

- a minimal `TrackingFs` that delegates the full `FileSystem` trait surface
- no persistence logic yet, only proof of wrapper compatibility

Checkpoint:

- wrapper compiles cleanly against `fuse-backend-rs`
- lookup/open/release-style methods can forward context and handles without
  patching upstream crates

### Step 4: DAX support

Implement:

- cache size config
- DAX mapping allocation in `device_manager`
- `VirtioSharedMemoryList`
- `CacheHandler`

Checkpoint:

- SHM BAR appears
- DAX-enabled guest mount works

### Step 5: Snapshot tracker

Implement:

- `TrackingFs`
- `SnapshotTracker`
- `FsState`
- `snapshot()` and restore path

Checkpoint:

- snapshot/restore succeeds for stable-path files

### Step 6: Seccomp hardening

Implement:

- new `Thread::VirtioFs`
- syscall filter

Checkpoint:

- integration tests pass with seccomp enabled

### Step 7: Test completion

Implement remaining tests and tighten error paths.

## Snapshot/Restore Semantics for POC

This POC will support snapshot/restore for the built-in backend under these
rules:

- tracked paths must still exist at restore time
- restore aborts on missing tracked paths
- open but unlinked files are not supported
- rename or replace outside of tracked visibility may cause restore failure
- negotiated virtio feature state is restored together with filesystem state

This is acceptable for the POC as long as the behavior is explicit and tested.

## Risks

### Risk 1: Path-based restore is semantically weak

Mitigation:

- mark the feature experimental
- fail restore rather than silently reopening the wrong object
- add explicit regression tests for unlink and replace-like scenarios

### Risk 2: DAX write semantics are security-sensitive

Mitigation:

- start with read-only DAX mmap behavior
- reject writable DAX mappings explicitly in `CacheHandler::map()`
- do not promise guest direct writable DAX in the POC

### Risk 3: Seccomp bring-up failures

Mitigation:

- stage the filter late in implementation
- validate under tests before tightening

### Risk 4: Fuse-backend-rs assumptions do not align exactly with CH threading

Mitigation:

- follow CH's `EpollHelperHandler + spawn_virtio_thread` pattern
- avoid trying to import Dragonball's runtime model wholesale

### Risk 5: Tracking wrapper may not be ergonomically compatible

Mitigation:

- require an early spike for full trait delegation
- stop and reassess before Step 5 if upstream trait forwarding is too invasive

## Verification Matrix

### Parsing and validation

- socket backend parses
- builtin backend parses
- invalid mixed backend config fails
- DAX without shared memory fails validation

### Functional I/O

- mount builtin virtio-fs
- read host file from guest
- write guest file to host shared dir

### DAX

- SHM BAR is exposed when `cache_size > 0`
- guest mount succeeds with DAX enabled

### Snapshot/restore

- snapshot and restore with existing file succeeds
- snapshot and restore with open regular file succeeds
- snapshot and restore with open unlinked file fails explicitly
- restored device preserves negotiated virtio feature state

### Security

- builtin backend limited to `shared_dir` via Landlock
- seccomp worker thread stays within allowlist

## Acceptance Criteria

The POC is complete when:

- both backend styles build and coexist
- builtin virtio-fs mounts and performs basic I/O
- DAX-enabled builtin mode exposes a valid shared memory BAR
- snapshot/restore works for supported path-stable cases
- unsupported restore cases fail explicitly, not silently
- existing vhost-user virtio-fs tests remain green
- feature is gated behind `native_virtiofs`

## Deferred Work After POC

- live migration for builtin fs
- file-handle-based snapshot state
- stronger restore identity guarantees
- guest-writable DAX policy
- performance tuning and thread pool optimization
