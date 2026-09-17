// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Transport-independent host coordination, evidence, and native assignment.
//!
//! This module does not implement a Linux backend, guest transport, attestation
//! verification, or an access gate. Adding it does not enable LOCK or RUN on
//! physical devices. Native states and objects have no wire discriminants;
//! adapters must translate protocol values explicitly.
//!
//! A future assignment owner must use one [`Coordinator`](crate::host::Coordinator)
//! per device and one shared [`SnapshotBudget`](crate::host::SnapshotBudget) per
//! VM. It must serialize all other device access
//! with this coordinator. In particular, the backend must revoke guest/host BAR,
//! DMA, and interrupt access as required **before** issuing mutations, and keep
//! access revoked after an error. A confirmed TDISP state alone does not grant
//! access. Backend aliases must not bypass this ownership contract.
//!
//! Calls are synchronous and cannot be cancelled in flight. Integration must
//! arrange execution away from critical async executor paths. There is no
//! implicit unlock or teardown on drop, no continuation support, and no generic
//! backend accessor. The assignment owner must arrange explicit teardown and
//! retain any resources needed to contain a quarantined device.
//!
//! [`Coordinator::into_evidence_service`](crate::host::Coordinator::into_evidence_service)
//! consumes the coordinator for bounded blocking-pool execution. Assignment
//! operations remain unavailable unless the backend explicitly implements them.

mod evidence;

pub use evidence::EvidenceError;
pub use evidence::EvidenceService;
pub use evidence::EvidenceSink;

use parking_lot::Mutex;
use std::collections::TryReserveError;
use std::sync::Arc;

/// Maximum bytes in one cached object.
pub const MAX_OBJECT_SIZE: usize = 16 * 1024 * 1024;

/// A device state confirmed by the backend, not a protocol enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmedState {
    /// The interface is not locked.
    Unlocked,
    /// The interface is locked but not running.
    Locked,
    /// The interface is running.
    Running,
}

/// Local lifecycle state. Quarantine must never be reported as a device state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    /// The last operation completed with a confirmed outcome.
    Confirmed(ConfirmedState),
    /// A mutation might have committed. Only explicit teardown is allowed.
    Quarantined {
        /// Historical state only; not a statement about current hardware.
        last_confirmed: ConfirmedState,
    },
    /// Explicit teardown completed successfully. No further operations are allowed.
    TornDown,
}

/// Native object identity, independent of guest or kernel numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Object {
    /// Device interface report, returned without rewriting.
    InterfaceReport,
    /// Measurement evidence, returned without verification.
    Measurements,
    /// Certificate object bytes, without assumptions about their format.
    Certificate,
    /// VCA evidence bytes.
    Vca,
}

impl Object {
    fn index(self) -> usize {
        match self {
            Self::InterfaceReport => 0,
            Self::Measurements => 1,
            Self::Certificate => 2,
            Self::Vca => 3,
        }
    }
}

/// Owned measurement input. Guest layout and flag decoding belong in adapters.
///
/// The raw/hash choice has native meaning, not a protobuf wire discriminant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasurementRequest {
    /// Challenge bytes, copied from validated guest memory by the adapter.
    pub nonce: [u8; 32],
    /// Request raw measurements (format 1), rather than hashes (format 0).
    pub raw: bool,
}

/// Shared RAM backing retained until an acknowledged IOAS unmap.
#[derive(Clone, Debug)]
pub struct SharedMapping {
    /// The VM's coherent shared backing, not a replacement RAM allocation.
    pub file: Arc<std::fs::File>,
    /// Offset in the backing file.
    pub file_offset: u64,
    /// Shared device-visible address, including the negotiated IPA selector.
    pub iova: u64,
    /// Page-aligned mapping length.
    pub length: u64,
}

/// DMA access borrowed from an already admitted assignment owner.
///
/// These calls must not acquire coordinator admission again. A failed call
/// quarantines access even if the RAM work subsequently returns success.
pub trait SharedDma {
    /// Map coherent shared backing and retain its ownership through unmap.
    fn map(
        &mut self,
        mapping: SharedMapping,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    /// Withdraw a fully tracked device-visible interval before making it private.
    fn unmap(
        &mut self,
        iova: u64,
        length: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    /// Confirm initial private import and prefault completed while the IOAS was empty.
    fn prepare_private_memory(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// One complete host-owned RAM conversion or initial preparation operation.
///
/// Runs synchronously on the assignment's blocking worker, with coordinator
/// admission and the frontend access gate already held. Obtain the VM memory
/// ledger only inside this call: the lock order is device, then memory.
/// Never call the device's asynchronous service from this method.
pub trait RamWork: Send + Sync {
    /// Complete all DMA withdrawal, backing conversion and prefault/mapping work.
    fn run(&self, dma: &mut dyn SharedDma) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Assignment changes serialized with evidence and state transitions.
pub enum AssignmentOperation {
    /// Execute a complete RAM operation under one coordinator admission.
    ConvertRam(Arc<dyn RamWork>),
    /// Revoke access after an external conversion or completion failure.
    ///
    /// This deliberately returns an error and leaves the coordinator quarantined.
    Quarantine,
    /// Stage protected device memory using only tracked BAR address translation.
    ///
    /// Success permits subsequent kernel/RMM validation; it is not proof that
    /// the guest accepted the mapping. Some kernels mask partial-map failures.
    ValidateMmio {
        /// First guest address.
        base: u64,
        /// Exclusive upper guest address.
        top: u64,
        /// Host address requested by the kernel/RMM exit. The backend must
        /// compare it with its fixed BAR translation before issuing a request.
        pa_base: u64,
    },
    /// Record a kernel-completed DEV-to-EMPTY invalidation.
    InvalidateMmio {
        /// First guest address.
        base: u64,
        /// Exclusive upper guest address.
        top: u64,
    },
    /// Acknowledge initial private import before admitting shared IOAS mappings.
    PreparePrivateMemory,
    /// Add shared RAM; retain its backing through unmap.
    MapShared(SharedMapping),
    /// Withdraw a complete tracked shared interval.
    UnmapShared {
        /// First device-visible address.
        iova: u64,
        /// Mapping length.
        length: u64,
    },
}

/// Supported regeneration requests. Other objects cannot be regenerated here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Regenerate {
    /// Generate a new interface report.
    InterfaceReport,
    /// Generate measurements using owned challenge bytes.
    Measurements(MeasurementRequest),
}

/// Mutation recorded in the latest transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// Change assignment mappings or acknowledge private-memory preparation.
    Assignment,
    /// Change the confirmed interface state.
    SetState(ConfirmedState),
    /// Regenerate an interface report.
    InterfaceReport,
    /// Regenerate measurements.
    Measurements,
    /// Reset and confirm an unlocked interface.
    Reset,
    /// Revoke access and tear down the assignment.
    Teardown,
}

/// Bounded transition history: the most recent attempted backend mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    /// State before the attempt.
    pub before: DeviceState,
    /// Requested operation.
    pub operation: Mutation,
    /// Confirmed result, or quarantine on error.
    pub after: DeviceState,
}

/// Synchronous backend owned exclusively by a [`Coordinator`].
///
/// Implementations must validate all native-to-platform translations. Successful
/// mutations must confirm completion, not merely submission. Every mutation error
/// is treated as potentially committed, including errors normally called
/// "validation errors" by a platform. Do not turn partial results into success.
///
/// State and object reads must not change device state or regenerate objects.
/// External changes are not authenticated or detected by the snapshot cache;
/// evidence validation remains the guest/RMM's responsibility.
pub trait Backend {
    /// Typed platform failure, retained as the error source.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Check containment failures recorded by a synchronized frontend.
    ///
    /// This must not issue a mutation or clear an earlier failure.
    fn check_access(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Whether this backend implements native assignment operations.
    fn supports_assignment(&self) -> bool {
        false
    }

    /// Perform an assignment change. `None` means unsupported without mutation.
    fn assignment(&mut self, _operation: AssignmentOperation) -> Option<Result<(), Self::Error>> {
        None
    }

    /// Query the initial confirmed state without modifying it.
    fn state(&mut self) -> Result<ConfirmedState, Self::Error>;
    /// Revoke access as required, issue the transition, and confirm completion.
    fn set_state(&mut self, state: ConfirmedState) -> Result<(), Self::Error>;
    /// Return the complete object length, with platform result channels checked.
    fn object_size(&mut self, object: Object) -> Result<u64, Self::Error>;
    /// Read the whole object at backend offset zero into initialized host memory.
    ///
    /// There is deliberately no offset argument. Return the actual byte count,
    /// not the offered buffer size. Never retain the buffer after this call.
    fn read_object(&mut self, object: Object, buffer: &mut [u8]) -> Result<usize, Self::Error>;
    /// Regenerate evidence and confirm completion.
    fn regenerate(&mut self, request: &Regenerate) -> Result<(), Self::Error>;
    /// Revoke access, reset, and confirm that the interface is unlocked.
    fn reset(&mut self) -> Result<(), Self::Error>;
    /// Revoke access and confirm complete assignment teardown.
    ///
    /// This must also work from an unknown device state. Failure must leave
    /// resources contained; neither this core nor its drop path retries it.
    fn teardown(&mut self) -> Result<(), Self::Error>;
}

/// Shared upper bound for all cached object storage in one VM.
///
/// Clones share accounting. Create this once at the assignment-owner boundary,
/// not once per device. Metadata is fixed-size per coordinator and is not charged.
#[derive(Debug, Clone)]
pub struct SnapshotBudget(Arc<Mutex<BudgetState>>);

#[derive(Debug)]
struct BudgetState {
    limit: usize,
    used: usize,
}

impl SnapshotBudget {
    /// Create a VM-wide budget, in bytes. Zero disables snapshot acquisition.
    pub fn new(limit: usize) -> Self {
        Self(Arc::new(Mutex::new(BudgetState { limit, used: 0 })))
    }

    /// Bytes currently reserved, including in-flight snapshot allocations.
    pub fn used(&self) -> usize {
        self.0.lock().used
    }

    fn reserve(&self, bytes: usize) -> Result<Reservation, SnapshotError> {
        let mut state = self.0.lock();
        if bytes > state.limit - state.used {
            return Err(SnapshotError::BudgetExceeded {
                requested: bytes,
                available: state.limit - state.used,
            });
        }
        state.used += bytes;
        Ok(Reservation {
            budget: self.clone(),
            bytes,
        })
    }
}

struct Reservation {
    budget: SnapshotBudget,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.0.lock().used -= self.bytes;
    }
}

struct Snapshot {
    bytes: Vec<u8>,
    _reservation: Reservation,
}

impl Snapshot {
    fn allocate(size: usize, budget: &SnapshotBudget) -> Result<Self, SnapshotError> {
        let mut reservation = budget.reserve(size)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(SnapshotError::Allocation)?;
        // Charge actual capacity if an allocator provides extra usable storage.
        let extra = bytes.capacity() - size;
        if extra != 0 {
            let mut additional = budget.reserve(extra)?;
            reservation.bytes += extra;
            additional.bytes = 0;
        }
        bytes.resize(size, 0);
        Ok(Self {
            bytes,
            _reservation: reservation,
        })
    }
}

/// Local snapshot validation or resource failure.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// A zero size cannot establish that an object is present.
    #[error("backend reported an empty or absent object")]
    EmptyObject,
    /// An object exceeds the per-object bound.
    #[error("object size {0} exceeds the 16 MiB limit")]
    TooLarge(u64),
    /// The shared VM budget cannot hold this object.
    #[error("snapshot needs {requested} bytes; only {available} bytes remain")]
    BudgetExceeded {
        /// Requested allocation.
        requested: usize,
        /// Unreserved budget.
        available: usize,
    },
    /// Host storage could not be allocated.
    #[error("snapshot allocation failed")]
    Allocation(#[source] TryReserveError),
    /// Backend size and read results disagree.
    #[error("backend returned {actual} bytes; queried size was {expected}")]
    IncoherentRead {
        /// Verified queried size.
        expected: usize,
        /// Backend's actual read result.
        actual: usize,
    },
    /// The requested slice overflows or exceeds the current snapshot.
    #[error("object slice offset {offset}, length {length} exceeds size {size}")]
    InvalidRange {
        /// Requested byte offset.
        offset: u64,
        /// Requested byte length.
        length: u64,
        /// Current snapshot length.
        size: usize,
    },
}

/// Native coordinator failure. No variant represents successful completion.
#[derive(Debug, thiserror::Error)]
pub enum Error<E: std::error::Error + 'static> {
    /// This owner does not implement native assignment operations.
    #[error("native assignment operation is unsupported")]
    Unsupported,
    /// A request was rejected locally without issuing any mutation.
    #[error("operation is not allowed in state {state:?}")]
    InvalidState {
        /// Current local lifecycle state.
        state: DeviceState,
    },
    /// A state transition was rejected locally, without implicit unlock.
    #[error("transition from {from:?} to {to:?} is not allowed")]
    InvalidTransition {
        /// Current confirmed state.
        from: ConfirmedState,
        /// Requested state.
        to: ConfirmedState,
    },
    /// A backend operation failed. For mutations, the device is quarantined.
    #[error("backend operation failed")]
    Backend(#[source] E),
    /// Snapshot acquisition or slicing failed.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

/// Exclusive per-device lifecycle and whole-object snapshot owner.
///
/// All operations require `&mut self`, including snapshot reads. Returned slices
/// borrow this owner, preventing mutation or budget release while they are in use.
/// Snapshots are keyed by object within the current generation: clearing all slots
/// before every mutation prevents mixing generations without a wrapping counter.
/// In-flight mutations provisionally quarantine the owner, including on unwind.
/// Exclusive borrowing prevents callers from observing an intermediate state.
pub struct Coordinator<B: Backend> {
    backend: B,
    state: DeviceState,
    last_transition: Option<Transition>,
    budget: SnapshotBudget,
    snapshots: [Option<Snapshot>; 4],
}

impl<B: Backend> Coordinator<B> {
    /// Whether the exclusively owned backend supports native assignment.
    pub fn supports_assignment(&self) -> bool {
        self.backend.supports_assignment()
    }

    /// Serialize a mapping change with the same quarantine and snapshot policy.
    pub fn assignment(&mut self, operation: AssignmentOperation) -> Result<(), Error<B::Error>> {
        let state = self.confirmed()?;
        if !self.backend.supports_assignment() {
            return Err(Error::Unsupported);
        }
        self.mutate(
            Mutation::Assignment,
            DeviceState::Confirmed(state),
            |backend| {
                backend
                    .assignment(operation)
                    .expect("assignment-capable backend implements assignment operations")
            },
        )
    }
    /// Take exclusive backend ownership and query its initial confirmed state.
    ///
    /// Failure drops the backend without implicit teardown. The assignment owner
    /// must retain independent containment resources when constructing this owner.
    pub fn new(mut backend: B, budget: SnapshotBudget) -> Result<Self, Error<B::Error>> {
        let state = backend.state().map_err(Error::Backend)?;
        Ok(Self {
            backend,
            state: DeviceState::Confirmed(state),
            last_transition: None,
            budget,
            snapshots: std::array::from_fn(|_| None),
        })
    }

    /// Current local lifecycle state, including any uncertain outcome.
    pub fn state(&self) -> DeviceState {
        self.state
    }

    /// The latest mutation attempt. Invalid local requests do not replace it.
    pub fn last_transition(&self) -> Option<Transition> {
        self.last_transition
    }

    fn confirmed(&mut self) -> Result<ConfirmedState, Error<B::Error>> {
        if let DeviceState::Confirmed(last_confirmed) = self.state {
            if let Err(error) = self.backend.check_access() {
                self.invalidate();
                self.state = DeviceState::Quarantined { last_confirmed };
                return Err(Error::Backend(error));
            }
        }
        match self.state {
            DeviceState::Confirmed(state) => Ok(state),
            state => Err(Error::InvalidState { state }),
        }
    }

    fn invalidate(&mut self) {
        self.snapshots = std::array::from_fn(|_| None);
    }

    fn mutate(
        &mut self,
        operation: Mutation,
        after: DeviceState,
        call: impl FnOnce(&mut B) -> Result<(), B::Error>,
    ) -> Result<(), Error<B::Error>> {
        let before = self.state;
        let last_confirmed = match before {
            DeviceState::Confirmed(state)
            | DeviceState::Quarantined {
                last_confirmed: state,
            } => state,
            state => return Err(Error::InvalidState { state }),
        };
        self.invalidate();
        // Quarantine first so unwinding cannot leave a success-shaped state.
        self.state = DeviceState::Quarantined { last_confirmed };
        self.last_transition = Some(Transition {
            before,
            operation,
            after: self.state,
        });
        let result = call(&mut self.backend);
        if result.is_ok() {
            self.state = after;
            self.last_transition = Some(Transition {
                before,
                operation,
                after,
            });
        }
        result.map_err(Error::Backend)
    }

    /// Perform an explicit transition. Repeated states and UNLOCKED → RUN fail
    /// locally. Invalid requests neither call the backend nor invalidate snapshots.
    pub fn set_state(&mut self, to: ConfirmedState) -> Result<(), Error<B::Error>> {
        let from = self.confirmed()?;
        if !matches!(
            (from, to),
            (ConfirmedState::Unlocked, ConfirmedState::Locked)
                | (ConfirmedState::Locked, ConfirmedState::Running)
                | (ConfirmedState::Locked, ConfirmedState::Unlocked)
                | (ConfirmedState::Running, ConfirmedState::Unlocked)
        ) {
            return Err(Error::InvalidTransition { from, to });
        }
        self.mutate(Mutation::SetState(to), DeviceState::Confirmed(to), |b| {
            b.set_state(to)
        })
    }

    /// Regenerate evidence in LOCKED or RUN. All cached objects are invalidated
    /// before the call. A backend failure quarantines the device.
    pub fn regenerate(&mut self, request: Regenerate) -> Result<(), Error<B::Error>> {
        let state = self.confirmed()?;
        if state == ConfirmedState::Unlocked {
            return Err(Error::InvalidState { state: self.state });
        }
        let operation = match &request {
            Regenerate::InterfaceReport => Mutation::InterfaceReport,
            Regenerate::Measurements(_) => Mutation::Measurements,
        };
        self.mutate(operation, DeviceState::Confirmed(state), |b| {
            b.regenerate(&request)
        })
    }

    /// Explicitly reset a confirmed device. Quarantine is not recoverable by a
    /// state query or reset; it requires assignment teardown.
    pub fn reset(&mut self) -> Result<(), Error<B::Error>> {
        self.confirmed()?;
        self.mutate(
            Mutation::Reset,
            DeviceState::Confirmed(ConfirmedState::Unlocked),
            B::reset,
        )
    }

    /// Explicit teardown from a confirmed or quarantined state. Errors preserve
    /// quarantine. Dropping this owner does not call this method.
    pub fn teardown(&mut self) -> Result<(), Error<B::Error>> {
        self.mutate(Mutation::Teardown, DeviceState::TornDown, B::teardown)
    }

    fn acquire(&mut self, object: Object) -> Result<&[u8], Error<B::Error>> {
        self.confirmed()?;
        let index = object.index();
        if self.snapshots[index].is_none() {
            match self.fetch(object) {
                Ok(snapshot) => self.snapshots[index] = Some(snapshot),
                Err(error) => {
                    self.invalidate();
                    // A synchronized physical frontend can latch containment
                    // failure during a read. Preserve the original read error,
                    // but reflect that quarantine before releasing admission.
                    let _ = self.confirmed();
                    return Err(error);
                }
            }
        }
        // This slot was either populated above or was already present.
        Ok(&self.snapshots[index].as_ref().unwrap().bytes)
    }

    fn fetch(&mut self, object: Object) -> Result<Snapshot, Error<B::Error>> {
        let size = self.backend.object_size(object).map_err(Error::Backend)?;
        if size == 0 {
            return Err(SnapshotError::EmptyObject.into());
        }
        if size > MAX_OBJECT_SIZE as u64 {
            return Err(SnapshotError::TooLarge(size).into());
        }
        let size = size as usize;
        let mut snapshot = Snapshot::allocate(size, &self.budget)?;
        let actual = self
            .backend
            .read_object(object, &mut snapshot.bytes)
            .map_err(Error::Backend)?;
        if actual != size {
            return Err(SnapshotError::IncoherentRead {
                expected: size,
                actual,
            }
            .into());
        }
        Ok(snapshot)
    }

    /// Acquire a whole-object snapshot if needed and return its verified size.
    ///
    /// Zero-sized objects are rejected: a backend may use zero for an absent
    /// object, which must not become a success-shaped evidence snapshot.
    pub fn object_size(&mut self, object: Object) -> Result<usize, Error<B::Error>> {
        Ok(self.acquire(object)?.len())
    }

    /// Serve a checked byte slice of the same snapshot used by [`Self::object_size`].
    ///
    /// Offsets are never passed to the backend. Zero-length reads at the end are
    /// valid. Bytes are preserved exactly; this method makes no attestation claim.
    pub fn read_object(
        &mut self,
        object: Object,
        offset: u64,
        length: u64,
    ) -> Result<&[u8], Error<B::Error>> {
        let bytes = self.acquire(object)?;
        let end = offset
            .checked_add(length)
            .filter(|&end| end <= bytes.len() as u64)
            .ok_or(SnapshotError::InvalidRange {
                offset,
                length,
                size: bytes.len(),
            })?;
        Ok(&bytes[offset as usize..end as usize])
    }
}

#[cfg(test)]
mod tests;
