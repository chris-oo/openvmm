// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM interaction handle encapsulating all RPC channels to a running VM.

use crate::serial_buffer::SerialRingBuffer;
use mesh::rpc::RpcSend;
use openvmm_defs::rpc::VmRpc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

/// Handle to a running VM, providing RPC channels and state tracking.
///
/// This is the primary interface tools use to interact with the VM. It wraps
/// the various mesh channels established during VM launch.
pub struct VmHandle {
    /// RPC sender for VM lifecycle operations (pause, resume, reset, etc.).
    pub vm_rpc: mesh::Sender<VmRpc>,
    /// Worker handle for the VM worker process.
    pub worker: mesh_worker::WorkerHandle,
    /// Serial output ring buffer shared with the serial output sink.
    pub serial_buffer: Arc<SerialRingBuffer>,
    /// Serial console input writer (COM1) — synchronous writer for use from
    /// any thread/context.
    pub console_in: parking_lot::Mutex<Option<Box<dyn std::io::Write + Send>>>,
   /// Paravisor diagnostic client for `pv/inspect` operations.
   pub diag_client: Option<Arc<diag_client::DiagClient>>,
    /// Whether the VM is currently halted.
    halted: AtomicBool,
    /// Whether the VM is currently paused.
    paused: AtomicBool,
    /// Human-readable halt reason, if any.
    halt_reason: parking_lot::Mutex<Option<String>>,
    /// Senders waiting for a halt notification. Drained on halt.
    halt_waiters: parking_lot::Mutex<Vec<mesh::Sender<String>>>,
}

impl VmHandle {
    /// Create a new `VmHandle`.
    pub fn new(
        vm_rpc: mesh::Sender<VmRpc>,
        worker: mesh_worker::WorkerHandle,
        serial_buffer: Arc<SerialRingBuffer>,
        console_in: Option<Box<dyn std::io::Write + Send>>,
       diag_client: Option<Arc<diag_client::DiagClient>>,
    ) -> Self {
        Self {
            vm_rpc,
            worker,
            serial_buffer,
            console_in: parking_lot::Mutex::new(console_in),
           diag_client,
            halted: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            halt_reason: parking_lot::Mutex::new(None),
            halt_waiters: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// Record that the VM has halted with the given reason string.
    ///
    /// Also notifies any pending `vm/wait_for_halt` callers.
    pub fn set_halted(&self, reason: String) {
        *self.halt_reason.lock() = Some(reason.clone());
        self.halted.store(true, Ordering::Release);
        // A halted VM is not paused.
        self.paused.store(false, Ordering::Release);
        // Notify all pending halt waiters.
        let waiters: Vec<_> = self.halt_waiters.lock().drain(..).collect();
        for waiter in waiters {
            waiter.send(reason.clone());
        }
    }

    /// Clear the halted state (e.g., after a `ClearHalt` RPC).
    pub fn clear_halted(&self) {
        *self.halt_reason.lock() = None;
        self.halted.store(false, Ordering::Release);
    }

    /// Returns `true` if the VM is currently halted.
    pub fn is_halted(&self) -> bool {
        self.halted.load(Ordering::Acquire)
    }

    /// Record whether the VM is paused.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    /// Returns `true` if the VM is currently paused.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Register to be notified when the VM halts.
    ///
    /// Returns a receiver that will receive the halt reason string when the VM
    /// halts. If the VM is already halted, the caller should check
    /// [`is_halted`](Self::is_halted) first.
    pub fn register_halt_waiter(&self) -> mesh::Receiver<String> {
        let (tx, rx) = mesh::channel();
        self.halt_waiters.lock().push(tx);
        rx
    }

    /// Returns the current halt reason, if any.
    pub fn halt_reason_string(&self) -> Option<String> {
        self.halt_reason.lock().clone()
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
    /// Test the halt waiter notification mechanism in isolation using the same
    /// primitives (`parking_lot::Mutex<Vec<mesh::Sender<String>>>`) that
    /// `VmHandle` uses.
    #[test]
    fn halt_waiters_are_notified() {
        let waiters: parking_lot::Mutex<Vec<mesh::Sender<String>>> =
            parking_lot::Mutex::new(Vec::new());

        // Register two waiters.
        let (tx1, mut rx1) = mesh::channel();
        let (tx2, mut rx2) = mesh::channel();
        waiters.lock().push(tx1);
        waiters.lock().push(tx2);

        // Drain and notify — mirrors VmHandle::set_halted().
        let reason = "triple fault".to_string();
        let drained: Vec<_> = waiters.lock().drain(..).collect();
        for w in drained {
            w.send(reason.clone());
        }

        // Both receivers should get the reason.
        use futures::StreamExt;
        let msg1 = futures::executor::block_on(rx1.next());
        let msg2 = futures::executor::block_on(rx2.next());
        assert_eq!(msg1.as_deref(), Some("triple fault"));
        assert_eq!(msg2.as_deref(), Some("triple fault"));
    }

    #[test]
    fn halt_waiters_empty_after_drain() {
        let waiters: parking_lot::Mutex<Vec<mesh::Sender<String>>> =
            parking_lot::Mutex::new(Vec::new());

        let (tx, _rx) = mesh::channel::<String>();
        waiters.lock().push(tx);

        // Drain.
        let drained: Vec<_> = waiters.lock().drain(..).collect();
        assert_eq!(drained.len(), 1);

        // List is now empty.
        assert!(waiters.lock().is_empty());
    }
}
