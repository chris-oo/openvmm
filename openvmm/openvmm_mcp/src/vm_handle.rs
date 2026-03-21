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
    /// Whether the VM is currently halted.
    halted: Arc<AtomicBool>,
    /// Human-readable halt reason, if any.
    halt_reason: Arc<std::sync::Mutex<Option<String>>>,
}

impl VmHandle {
    /// Create a new `VmHandle`.
    pub fn new(
        vm_rpc: mesh::Sender<VmRpc>,
        worker: mesh_worker::WorkerHandle,
        serial_buffer: Arc<SerialRingBuffer>,
    ) -> Self {
        Self {
            vm_rpc,
            worker,
            serial_buffer,
            halted: Arc::new(AtomicBool::new(false)),
            halt_reason: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Record that the VM has halted with the given reason string.
    pub fn set_halted(&self, reason: String) {
        *self.halt_reason.lock().unwrap() = Some(reason);
        self.halted.store(true, Ordering::Release);
    }

    /// Clear the halted state (e.g., after a `ClearHalt` RPC).
    pub fn clear_halted(&self) {
        *self.halt_reason.lock().unwrap() = None;
        self.halted.store(false, Ordering::Release);
    }

    /// Returns `true` if the VM is currently halted.
    pub fn is_halted(&self) -> bool {
        self.halted.load(Ordering::Acquire)
    }

    /// Returns the current halt reason, if any.
    pub fn halt_reason_string(&self) -> Option<String> {
        self.halt_reason.lock().unwrap().clone()
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
