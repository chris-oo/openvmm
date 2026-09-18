// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Platform- and protocol-neutral lifecycle admission and mutation outcomes.
//!
//! Facades own their backends and containment policy. An admitted synchronous
//! mutation provisionally quarantines this engine before invoking facade work.
//! Only successful completion commits the operation's prescribed outcome.

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

/// Mutation recorded in the latest transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// Change assignment mappings or acknowledge private-memory preparation.
    Assignment,
    /// Change the confirmed interface state. Repeated states are invalid.
    SetState(ConfirmedState),
    /// Unbind a healthy interface, including one already unlocked.
    Unbind,
    /// Modify an MMIO range on a locked or running interface.
    ModifyMmio,
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

/// A local rejection, before any mutation work.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AdmissionError {
    #[error("operation is not allowed in state {state:?}")]
    InvalidState { state: DeviceState },
    #[error("transition from {from:?} to {to:?} is not allowed")]
    InvalidTransition {
        from: ConfirmedState,
        to: ConfirmedState,
    },
}

/// Separate local rejection from uncertain backend completion.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MutationError<E: std::error::Error + 'static> {
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error("backend operation failed")]
    Backend(#[source] E),
}

/// The single authority for confirmed, quarantined, and terminal state.
pub(crate) struct Lifecycle {
    state: DeviceState,
    last_transition: Option<Transition>,
}

impl Lifecycle {
    pub(crate) fn new(state: ConfirmedState) -> Self {
        Self {
            state: DeviceState::Confirmed(state),
            last_transition: None,
        }
    }

    pub(crate) fn state(&self) -> DeviceState {
        self.state
    }

    pub(crate) fn last_transition(&self) -> Option<Transition> {
        self.last_transition
    }

    pub(crate) fn confirmed(&self) -> Result<ConfirmedState, AdmissionError> {
        match self.state {
            DeviceState::Confirmed(state) => Ok(state),
            state => Err(AdmissionError::InvalidState { state }),
        }
    }

    /// Latch an external containment failure without inventing a mutation.
    pub(crate) fn quarantine(&mut self) {
        if let DeviceState::Confirmed(last_confirmed) = self.state {
            self.state = DeviceState::Quarantined { last_confirmed };
        }
    }

    pub(crate) fn locked_or_running(&self) -> Result<(), AdmissionError> {
        match self.confirmed()? {
            ConfirmedState::Locked | ConfirmedState::Running => Ok(()),
            ConfirmedState::Unlocked => Err(AdmissionError::InvalidState { state: self.state }),
        }
    }

    fn outcome(&self, operation: Mutation) -> Result<DeviceState, AdmissionError> {
        if operation == Mutation::Teardown {
            return match self.state {
                DeviceState::Confirmed(_) | DeviceState::Quarantined { .. } => {
                    Ok(DeviceState::TornDown)
                }
                state => Err(AdmissionError::InvalidState { state }),
            };
        }
        let from = self.confirmed()?;
        let after = match operation {
            Mutation::SetState(to) => {
                if !matches!(
                    (from, to),
                    (ConfirmedState::Unlocked, ConfirmedState::Locked)
                        | (ConfirmedState::Locked, ConfirmedState::Running)
                        | (ConfirmedState::Locked, ConfirmedState::Unlocked)
                        | (ConfirmedState::Running, ConfirmedState::Unlocked)
                ) {
                    return Err(AdmissionError::InvalidTransition { from, to });
                }
                to
            }
            Mutation::InterfaceReport | Mutation::Measurements | Mutation::ModifyMmio => {
                self.locked_or_running()?;
                from
            }
            Mutation::Assignment => from,
            Mutation::Reset | Mutation::Unbind => ConfirmedState::Unlocked,
            Mutation::Teardown => unreachable!("teardown handled before confirmed admission"),
        };
        Ok(DeviceState::Confirmed(after))
    }

    /// Admit typed work and commit only its prescribed successful outcome.
    ///
    /// Rejected requests do not invoke `work` or replace history. Error or unwind
    /// leaves quarantine. Only owner teardown can enter from quarantine, and
    /// successful teardown is terminal. Callers cannot supply an arbitrary state.
    pub(crate) fn execute<E: std::error::Error + 'static>(
        &mut self,
        operation: Mutation,
        work: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), MutationError<E>> {
        let after = self.outcome(operation)?;
        let before = self.state;
        self.quarantine();
        self.last_transition = Some(Transition {
            before,
            operation,
            after: self.state,
        });
        work().map_err(MutationError::Backend)?;
        self.state = after;
        self.last_transition = Some(Transition {
            before,
            operation,
            after,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[derive(Debug, thiserror::Error)]
    #[error("injected backend failure")]
    struct Failure;

    fn succeed() -> Result<(), Failure> {
        Ok(())
    }

    fn operations() -> [Mutation; 10] {
        [
            Mutation::SetState(ConfirmedState::Unlocked),
            Mutation::SetState(ConfirmedState::Locked),
            Mutation::SetState(ConfirmedState::Running),
            Mutation::Assignment,
            Mutation::Unbind,
            Mutation::ModifyMmio,
            Mutation::InterfaceReport,
            Mutation::Measurements,
            Mutation::Reset,
            Mutation::Teardown,
        ]
    }

    #[test]
    fn confirmed_operation_matrix() {
        use ConfirmedState::*;
        for from in [Unlocked, Locked, Running] {
            for operation in operations() {
                let mut lifecycle = Lifecycle::new(from);
                let expected = match (from, operation) {
                    (Unlocked, Mutation::SetState(Locked)) => Some(DeviceState::Confirmed(Locked)),
                    (Locked, Mutation::SetState(Running)) => Some(DeviceState::Confirmed(Running)),
                    (Locked | Running, Mutation::SetState(Unlocked))
                    | (_, Mutation::Reset | Mutation::Unbind) => {
                        Some(DeviceState::Confirmed(Unlocked))
                    }
                    (_, Mutation::Assignment)
                    | (
                        Locked | Running,
                        Mutation::InterfaceReport | Mutation::Measurements | Mutation::ModifyMmio,
                    ) => Some(DeviceState::Confirmed(from)),
                    (_, Mutation::Teardown) => Some(DeviceState::TornDown),
                    _ => None,
                };
                let mut calls = 0;
                let result = lifecycle.execute(operation, || {
                    calls += 1;
                    succeed()
                });
                if let Some(after) = expected {
                    result.unwrap();
                    assert_eq!(calls, 1);
                    assert_eq!(lifecycle.state(), after);
                    assert_eq!(
                        lifecycle.last_transition(),
                        Some(Transition {
                            before: DeviceState::Confirmed(from),
                            operation,
                            after,
                        })
                    );
                } else {
                    assert!(matches!(result, Err(MutationError::Admission(_))));
                    assert_eq!(calls, 0);
                    assert_eq!(lifecycle.state(), DeviceState::Confirmed(from));
                    assert_eq!(lifecycle.last_transition(), None);
                }
            }
        }
    }

    #[test]
    fn error_or_unwind_requires_acknowledged_teardown() {
        for unwind in [false, true] {
            let mut lifecycle = Lifecycle::new(ConfirmedState::Running);
            if unwind {
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        lifecycle.execute(Mutation::Assignment, || -> Result<(), Failure> {
                            panic!("injected backend unwind");
                        })
                    }))
                    .is_err()
                );
            } else {
                assert!(matches!(
                    lifecycle.execute(Mutation::Assignment, || Err(Failure)),
                    Err(MutationError::Backend(Failure))
                ));
            }
            let quarantined = DeviceState::Quarantined {
                last_confirmed: ConfirmedState::Running,
            };
            let failed = Some(Transition {
                before: DeviceState::Confirmed(ConfirmedState::Running),
                operation: Mutation::Assignment,
                after: quarantined,
            });
            assert_eq!(lifecycle.state(), quarantined);
            assert_eq!(lifecycle.last_transition(), failed);
            assert!(lifecycle.confirmed().is_err());
            for operation in operations() {
                if operation != Mutation::Teardown {
                    assert!(matches!(
                        lifecycle.execute(operation, || -> Result<(), Failure> {
                            panic!("quarantined work must not run");
                        }),
                        Err(MutationError::Admission(
                            AdmissionError::InvalidState { .. }
                        ))
                    ));
                    assert_eq!(lifecycle.state(), quarantined);
                    assert_eq!(lifecycle.last_transition(), failed);
                }
            }
            assert!(matches!(
                lifecycle.execute(Mutation::Teardown, || Err(Failure)),
                Err(MutationError::Backend(Failure))
            ));
            assert_eq!(lifecycle.state(), quarantined);
            assert_eq!(
                lifecycle.last_transition(),
                Some(Transition {
                    before: quarantined,
                    operation: Mutation::Teardown,
                    after: quarantined,
                })
            );
            lifecycle.execute(Mutation::Teardown, succeed).unwrap();
            let terminal = Some(Transition {
                before: quarantined,
                operation: Mutation::Teardown,
                after: DeviceState::TornDown,
            });
            assert_eq!(lifecycle.last_transition(), terminal);
            lifecycle.quarantine();
            for operation in operations() {
                assert!(matches!(
                    lifecycle.execute(operation, || -> Result<(), Failure> {
                        panic!("terminal work must not run");
                    }),
                    Err(MutationError::Admission(
                        AdmissionError::InvalidState { .. }
                    ))
                ));
                assert_eq!(lifecycle.state(), DeviceState::TornDown);
                assert_eq!(lifecycle.last_transition(), terminal);
            }
        }
    }

    #[test]
    fn external_quarantine_preserves_last_mutation() {
        let mut lifecycle = Lifecycle::new(ConfirmedState::Unlocked);
        lifecycle
            .execute(Mutation::SetState(ConfirmedState::Locked), succeed)
            .unwrap();
        let transition = lifecycle.last_transition();
        lifecycle.quarantine();
        lifecycle.quarantine();
        assert_eq!(
            lifecycle.state(),
            DeviceState::Quarantined {
                last_confirmed: ConfirmedState::Locked,
            }
        );
        assert_eq!(lifecycle.last_transition(), transition);
    }
}
