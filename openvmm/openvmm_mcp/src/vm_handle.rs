// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM interaction handle encapsulating all RPC channels to a running VM.

use crate::serial_buffer::SerialRingBuffer;
use mesh::rpc::RpcSend;
use openvmm_defs::rpc::VmRpc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// Result of attempting to register a halt waiter.
pub enum HaltWaiterResult {
    /// The VM has already halted (or the worker has stopped). The reason
    /// string is returned directly — no need to wait.
    AlreadyHalted(String),
    /// A waiter was registered. The receiver will fire when the VM halts.
    Registered(mesh::Receiver<String>),
}

/// Combined halt and worker state, protected by a single lock to prevent
/// lost-wakeup races between `register_halt_waiter` and `set_halted`.
struct VmState {
    halted: bool,
    halt_reason: Option<String>,
    worker_stopped: bool,
    worker_error: Option<String>,
    halt_waiters: Vec<mesh::Sender<String>>,
}

/// Handle to a running VM, providing RPC channels and state tracking.
///
/// This is the primary interface tools use to interact with the VM. It wraps
/// the various mesh channels established during VM launch. Lifecycle
/// operations (pause, resume, reset, etc.) go directly to the VM worker via
/// [`VmRpc`]. Inspection is routed through the VM controller via a deferred
/// channel.
pub struct VmHandle {
    /// RPC sender for VM lifecycle operations (pause, resume, reset, etc.).
    pub vm_rpc: mesh::Sender<VmRpc>,
    /// Channel for sending inspect deferrals to the VM controller.
    pub inspect_fn: Box<dyn Fn(inspect::Deferred) + Send + Sync>,
    /// Serial output ring buffer shared with the serial output sink.
    pub serial_buffer: Arc<SerialRingBuffer>,
    /// Serial console input writer (COM1).
    pub console_in: parking_lot::Mutex<Option<Box<dyn std::io::Write + Send>>>,
    /// Whether the VM is currently paused (best-effort tracking).
    paused: AtomicBool,
    /// Combined halt and worker state.
    state: parking_lot::Mutex<VmState>,
}

impl VmHandle {
    /// Create a new `VmHandle`.
    pub fn new(
        vm_rpc: mesh::Sender<VmRpc>,
        serial_buffer: Arc<SerialRingBuffer>,
        console_in: Option<Box<dyn std::io::Write + Send>>,
        inspect_fn: Box<dyn Fn(inspect::Deferred) + Send + Sync>,
    ) -> Self {
        Self {
            vm_rpc,
            inspect_fn,
            serial_buffer,
            console_in: parking_lot::Mutex::new(console_in),
            paused: AtomicBool::new(false),
            state: parking_lot::Mutex::new(VmState {
                halted: false,
                halt_reason: None,
                worker_stopped: false,
                worker_error: None,
                halt_waiters: Vec::new(),
            }),
        }
    }

    /// Record that the VM has halted with the given reason string.
    ///
    /// Drains all pending halt waiters, sending them the reason.
    pub fn set_halted(&self, reason: String) {
        let mut state = self.state.lock();
        state.halt_reason = Some(reason.clone());
        state.halted = true;
        // A halted VM is not paused.
        self.paused.store(false, Ordering::Release);
        // Notify all pending halt waiters.
        let waiters: Vec<_> = state.halt_waiters.drain(..).collect();
        drop(state);
        for waiter in waiters {
            waiter.send(reason.clone());
        }
    }

    /// Record that the VM worker has stopped.
    ///
    /// Drains all pending halt waiters with a worker-stopped message.
    pub fn set_worker_stopped(&self, error: Option<String>) {
        let mut state = self.state.lock();
        state.worker_stopped = true;
        state.worker_error = error.clone();
        let msg = match &error {
            Some(e) => format!("worker stopped: {e}"),
            None => "worker stopped".to_string(),
        };
        let waiters: Vec<_> = state.halt_waiters.drain(..).collect();
        drop(state);
        for waiter in waiters {
            waiter.send(msg.clone());
        }
    }

    /// Clear the halted state (e.g., after a `ClearHalt` RPC).
    pub fn clear_halted(&self) {
        let mut state = self.state.lock();
        state.halted = false;
        state.halt_reason = None;
    }

    /// Returns `true` if the VM is currently halted.
    pub fn is_halted(&self) -> bool {
        self.state.lock().halted
    }

    /// Returns `true` if the VM worker has stopped.
    pub fn is_worker_stopped(&self) -> bool {
        self.state.lock().worker_stopped
    }

    /// Record whether the VM is paused.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    /// Returns `true` if the VM is currently paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Atomically check halt/worker state and register a halt waiter.
    ///
    /// If the VM is already halted or the worker has stopped, the reason is
    /// returned immediately. Otherwise a waiter is registered under the same
    /// lock, preventing lost-wakeup races.
    pub fn register_halt_waiter(&self) -> HaltWaiterResult {
        let mut state = self.state.lock();
        if state.halted {
            return HaltWaiterResult::AlreadyHalted(
                state
                    .halt_reason
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            );
        }
        if state.worker_stopped {
            return HaltWaiterResult::AlreadyHalted(
                state
                    .worker_error
                    .as_ref()
                    .map(|e| format!("worker stopped: {e}"))
                    .unwrap_or_else(|| "worker stopped".to_string()),
            );
        }
        let (tx, rx) = mesh::channel();
        state.halt_waiters.push(tx);
        HaltWaiterResult::Registered(rx)
    }

    /// Returns the current halt reason, if any.
    pub fn halt_reason_string(&self) -> Option<String> {
        self.state.lock().halt_reason.clone()
    }

    /// Returns the current VM status as a string.
    pub fn status_string(&self) -> &'static str {
        let state = self.state.lock();
        if state.worker_stopped {
            "worker_stopped"
        } else if state.halted {
            "halted"
        } else if self.paused.load(Ordering::Acquire) {
            "paused"
        } else {
            "running"
        }
    }

    /// Pause the VM. Returns `true` if the state actually changed.
    pub async fn pause(&self) -> anyhow::Result<bool> {
        Ok(self.vm_rpc.call(VmRpc::Pause, ()).await?)
    }

    /// Resume the VM. Returns `true` if the state actually changed.
    pub async fn resume(&self) -> anyhow::Result<bool> {
        Ok(self.vm_rpc.call(VmRpc::Resume, ()).await?)
    }

    /// Reset the VM.
    pub async fn reset(&self) -> anyhow::Result<()> {
        let result = self.vm_rpc.call(VmRpc::Reset, ()).await?;
        result.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    }

    /// Send an NMI to the specified virtual processor.
    pub async fn nmi(&self, vp: u32) -> anyhow::Result<()> {
        self.vm_rpc.call(VmRpc::Nmi, vp).await?;
        Ok(())
    }

    /// Clear a VM halt, allowing it to be resumed.
    pub async fn clear_halt(&self) -> anyhow::Result<bool> {
        let result = self.vm_rpc.call(VmRpc::ClearHalt, ()).await?;
        if result {
            self.clear_halted();
        }
        Ok(result)
    }

    /// Read guest physical memory at `gpa` for `size` bytes.
    pub async fn read_memory(&self, gpa: u64, size: usize) -> anyhow::Result<Vec<u8>> {
        let data = self.vm_rpc.call(VmRpc::ReadMemory, (gpa, size)).await?;
        data.map_err(|e| anyhow::anyhow!("{e}"))
    }

    /// Write `data` to guest physical memory at `gpa`.
    pub async fn write_memory(&self, gpa: u64, data: Vec<u8>) -> anyhow::Result<()> {
        let result = self.vm_rpc.call(VmRpc::WriteMemory, (gpa, data)).await?;
        result.map_err(|e| anyhow::anyhow!("{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial_buffer::SerialRingBuffer;

    fn test_vm_handle() -> VmHandle {
        let (vm_rpc, _vm_rpc_recv) = mesh::channel();
        VmHandle::new(
            vm_rpc,
            Arc::new(SerialRingBuffer::new()),
            None,
            Box::new(|_| {}),
        )
    }

    /// Registering a halt waiter on an already-halted VM returns the reason
    /// immediately, without going through the receiver.
    #[test]
    fn halt_waiter_race_free() {
        let vm = test_vm_handle();
        vm.set_halted("triple fault".to_string());

        match vm.register_halt_waiter() {
            HaltWaiterResult::AlreadyHalted(reason) => {
                assert_eq!(reason, "triple fault");
            }
            HaltWaiterResult::Registered(_) => {
                panic!("should have returned AlreadyHalted");
            }
        }
    }

    /// Registering a halt waiter on a running VM returns a receiver that fires
    /// when the VM halts.
    #[test]
    fn halt_waiter_receives_after_registration() {
        let vm = test_vm_handle();

        let mut rx = match vm.register_halt_waiter() {
            HaltWaiterResult::Registered(rx) => rx,
            HaltWaiterResult::AlreadyHalted(_) => panic!("should not be halted"),
        };

        vm.set_halted("triple fault".to_string());

        use futures::StreamExt;
        let msg = futures::executor::block_on(rx.next());
        assert_eq!(msg.as_deref(), Some("triple fault"));
    }

    /// When the worker stops, pending halt waiters are drained with an error.
    #[test]
    fn worker_stopped_drains_halt_waiters() {
        let vm = test_vm_handle();

        let mut rx = match vm.register_halt_waiter() {
            HaltWaiterResult::Registered(rx) => rx,
            _ => panic!("should register"),
        };

        vm.set_worker_stopped(Some("boom".to_string()));

        use futures::StreamExt;
        let msg = futures::executor::block_on(rx.next());
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("worker stopped"));
    }

    /// After the worker has stopped, new halt waiter registrations return
    /// immediately with an error.
    #[test]
    fn worker_stopped_blocks_new_waiters() {
        let vm = test_vm_handle();
        vm.set_worker_stopped(None);

        match vm.register_halt_waiter() {
            HaltWaiterResult::AlreadyHalted(reason) => {
                assert!(reason.contains("worker stopped"));
            }
            HaltWaiterResult::Registered(_) => {
                panic!("should have returned error immediately");
            }
        }
    }

    /// After draining halt waiters, registering again returns AlreadyHalted.
    #[test]
    fn halt_waiters_empty_after_drain() {
        let vm = test_vm_handle();

        let _rx = match vm.register_halt_waiter() {
            HaltWaiterResult::Registered(rx) => rx,
            _ => panic!("should register"),
        };

        vm.set_halted("halt".to_string());

        // After draining, registering again should return AlreadyHalted.
        match vm.register_halt_waiter() {
            HaltWaiterResult::AlreadyHalted(_) => {}
            _ => panic!("should be halted"),
        }
    }
}
