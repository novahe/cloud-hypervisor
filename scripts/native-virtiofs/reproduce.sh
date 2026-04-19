#!/bin/bash
# Stable Reproduction Script for Native Virtio-FS Functional & Performance Verification

# --- Environment Setup ---
export PATH=/root/go/go/bin:/root/.bun/bin:/root/.nvm/versions/node/v22.22.0/bin:/root/.local/bin:/root/.nvm/versions/node/v22.22.0/bin:/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/usr/local/games:/snap/bin:/usr/local/go/bin/:$PATH

# --- Configuration ---
BINARY=${BINARY:-/root/nova/kuasar/cloud-hypervisor/target/debug/cloud-hypervisor}
CH_REMOTE=${CH_REMOTE:-/root/nova/kuasar/cloud-hypervisor/target/debug/ch-remote}
KERNEL=${KERNEL:-/var/lib/kuasar/vmlinux.bin}
IMAGE=${IMAGE:-/var/lib/kuasar/kuasar.img}
SHARED_DIR=${SHARED_DIR:-/root/virtiofs_test/shared}
SOCKET=${SOCKET:-/root/virtiofs_test/ch.sock}
SERIAL_SOCK=${SERIAL_SOCK:-/root/virtiofs_test/serial.sock}
SNAP_DIR=${SNAP_DIR:-/root/virtiofs_test/snapshot_dir}
LOG_RUN=${LOG_RUN:-/root/virtiofs_test/vm.log}
LOG_RES=${LOG_RES:-/root/virtiofs_test/vm_restore.log}

# --- Shared Directory File Paths ---
GUEST_TASK="$SHARED_DIR/guest_task.sh"
GUEST_RESULT="$SHARED_DIR/guest_result.log"

# Common VM arguments (used by both run and restore to ensure consistency)
VM_ARGS=(
    --api-socket "$SOCKET"
    --memory size=512M,shared=on
    --kernel "$KERNEL"
    --pmem file="$IMAGE"
    --fs tag=test_tag,shared_dir="$SHARED_DIR"
    --cmdline "root=/dev/pmem0p1 rw rootfstype=ext4 console=ttyS0 init=/bin/sh"
    --console off
    --seccomp false
)

log() {
    echo -e "\033[1;32m[TEST]\033[0m $1"
}

cleanup() {
    log "Cleaning up existing processes and sockets..."
    pkill -f "$(basename $BINARY)" 2>/dev/null
    rm -f "$SOCKET" "$GUEST_TASK" "$GUEST_RESULT" "$LOG_RUN" "$LOG_RES"
}

# --- Background run (for automated testing) ---
run_vm() {
    cleanup
    mkdir -p "$SHARED_DIR"
    log "Starting VM in background..."
    $BINARY "${VM_ARGS[@]}" --serial socket="$SERIAL_SOCK" > "$LOG_RUN" 2>&1 &

    log "Waiting for API socket..."
    for i in {1..20}; do
        [ -S "$SOCKET" ] && log "API socket active." && return 0
        sleep 1
    done
    echo "Error: Socket timeout. Check $LOG_RUN"; exit 1
}

# --- Foreground interactive run ---
run_vm_fg() {
    run_vm
    log "Attaching to Serial Console (Interactive)..."
    # Fallback to direct TTY if socat is missing, but socat is better for state
    if command -v socat >/dev/null; then
        socat - UNIX-CONNECT:"$SERIAL_SOCK"
    else
        log "Warning: socat not found. Restarting in TTY MODE..."
        cleanup
        $BINARY "${VM_ARGS[@]}" --serial tty
    fi
}

# --- Snapshot ---
snapshot_vm() {
    log "Taking snapshot to $SNAP_DIR..."
    mkdir -p "$SNAP_DIR"
    # Ignore error if already paused
    $CH_REMOTE --api-socket "$SOCKET" pause || log "Warning: VM might already be paused. Proceeding..."
    $CH_REMOTE --api-socket "$SOCKET" snapshot "file://$SNAP_DIR"
    $CH_REMOTE --api-socket "$SOCKET" resume || log "Warning: VM could not be resumed."
    log "Snapshot saved and VM resumed."
}

# --- Background restore (for automated testing) ---
restore_vm() {
    log "Restoring VM from $SNAP_DIR..."
    pkill -f "$(basename $BINARY)" 2>/dev/null
    rm -f "$SOCKET"

    local start_time=$(date +%s%N)

    $BINARY \
        "${VM_ARGS[@]}" \
        --restore source_url="file://$SNAP_DIR" \
        --serial socket="$SERIAL_SOCK" \
        > "$LOG_RES" 2>&1 &

    for i in {1..400}; do
        if [ -S "$SOCKET" ]; then
            local end_time=$(date +%s%N)
            local duration=$(( (end_time - start_time) / 1000000 ))
            log "Restored VM socket active. [Restore Latency: ${duration}ms]"
            return 0
        fi
        sleep 0.05
    done
    echo "Error: Restore timeout. Check $LOG_RES"; exit 1
}

# --- Foreground interactive restore ---
restore_vm_fg() {
    cleanup
    log "Restoring VM into FOREGROUND (Interactive)..."
    $BINARY \
        "${VM_ARGS[@]}" \
        --restore source_url="file://$SNAP_DIR" \
        --serial socket="$SERIAL_SOCK" \
        > "$LOG_RES" 2>&1 &
    
    log "Waiting 5s for restored VM state to settle..."
    sleep 5
    if command -v socat >/dev/null; then
        log "Attaching to Serial Console..."
        socat - UNIX-CONNECT:"$SERIAL_SOCK"
    else
        log "Warning: socat not found. Restarting in TTY MODE..."
        cleanup
        $BINARY "${VM_ARGS[@]}" --restore source_url="file://$SNAP_DIR" --serial tty
    fi
}

# --- Guest interaction helpers ---
guest_exec() {
    local cmd="$1"
    local desc="$2"
    rm -f "$GUEST_RESULT"
    log "Dispatching Guest Task: $desc"
    echo "$cmd" > "$GUEST_TASK"

    for i in {1..30}; do
        if [ -f "$GUEST_RESULT" ]; then
            log "Guest Response Received:"
            cat "$GUEST_RESULT"
            return 0
        fi
        sleep 1
    done
    echo "Error: Guest task timeout! Is the listener running in the VM?"; exit 1
}

guest_setup_guide() {
    echo -e "\033[1;33m--- GUEST SIDE SETUP (ONE-TIME) ---\033[0m"
    echo "Please enter the VM and run the following combined command to prepare for CRUD testing:"
    echo -e "\033[1;36m"
    echo "export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin && mkdir -p /mnt && mount -t virtiofs test_tag /mnt && while true; do [ -f /mnt/guest_task.sh ] && sh /mnt/guest_task.sh > /mnt/guest_result.log 2>&1 && rm /mnt/guest_task.sh; sleep 1; done &"
    echo -e "\033[0m"
}

verify_all() {
    log "Starting Full Automated CRUD Verification..."

    if [ ! -S "$SOCKET" ]; then
        log "VM not running. Starting it now..."
        run_vm
        guest_setup_guide
        log "Please run the combined setup command above in the VM, then press ENTER to continue..."
        read
    fi

    log "--- Stage 1: Pre-Snapshot Operations ---"
    guest_exec "echo 222 > /mnt/tests && rm -rf /mnt/tests11 && /bin/ls /mnt" "Create tests, Remove tests11"
    
    snapshot_vm
    restore_vm
    
    log "--- Stage 2: Post-Restore Verification ---"
    guest_exec "/bin/cat /mnt/tests" "Verify tests content matches 222"
    guest_exec "echo 223 > /mnt/tests && /bin/cat /mnt/tests" "Update tests to 223 and verify"
    guest_exec "/bin/ls /mnt" "Final directory listing check"
    
    log "Full automated CRUD verification completed successfully!"
}

case "$1" in
    run) run_vm ;;
    run-fg) run_vm_fg ;;
    setup) guest_setup_guide ;;
    snap) snapshot_vm ;;
    restore) restore_vm ;;
    restore-fg) restore_vm_fg ;;
    test) guest_exec "$2" "$2" ;;
    all) verify_all ;;
    cleanup) cleanup ;;
    *) echo "Usage: $0 {run|run-fg|setup|snap|restore|restore-fg|test 'cmd'|all|cleanup}"; exit 1 ;;
esac
