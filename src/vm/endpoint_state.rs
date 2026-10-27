// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free lifecycle state for one VM-owned vCPU endpoint.

use hyper::sync::atomic::{AtomicU8, Ordering};

const UNBOUND: u8 = 0;
const DORMANT: u8 = 1;
const STARTED: u8 = 2;
const TERMINAL_MEMORY_FAULT: u8 = 3;
const TERMINAL_MMIO: u8 = 4;
const TERMINAL_SYNCHRONOUS: u8 = 5;
const STOP_REQUESTED: u8 = 6;
const HARDWARE_DETACHED: u8 = 7;
const REAPED_GUEST_MEMORY_FAULT: u8 = 8;
const REAPED_GUEST_MMIO: u8 = 9;
const REAPED_GUEST_SYNCHRONOUS: u8 = 10;
const REAPED_ADMINISTRATIVE: u8 = 11;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TerminalReason {
    MemoryFault,
    Mmio,
    Synchronous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AdministrativeStopReason {
    Requested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ClosureReason {
    Guest(TerminalReason),
    Administrative(AdministrativeStopReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Lifecycle {
    Unbound,
    Dormant,
    Started,
    GuestTerminal(TerminalReason),
    StopRequested(AdministrativeStopReason),
    HardwareDetached(AdministrativeStopReason),
    Reaped(ClosureReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StateError {
    Closed(Lifecycle),
    Corrupt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GuestCloseOutcome {
    Published,
    AdministrativeStopPending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TransitionError {
    Unexpected(Lifecycle),
    Corrupt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StopRequestOutcome {
    Published,
    AlreadyRequested,
    GuestTerminal(TerminalReason),
    HardwareDetached,
    Reaped,
    Inactive,
}

/// One-way endpoint state shared by backend producers and the vCPU runner.
pub(super) struct EndpointState {
    state: AtomicU8,
}

impl EndpointState {
    pub(super) const fn unbound() -> Self {
        Self {
            state: AtomicU8::new(UNBOUND),
        }
    }

    pub(super) fn publish_dormant(&self) -> Result<(), TransitionError> {
        transition_exact(&self.state, UNBOUND, DORMANT)
    }

    pub(super) fn publish_started(&self) -> Result<(), TransitionError> {
        transition_exact(&self.state, DORMANT, STARTED)
    }

    pub(super) fn ensure_live(&self) -> Result<(), StateError> {
        match decode(self.state.load(Ordering::Acquire)) {
            Ok(Lifecycle::Dormant | Lifecycle::Started) => Ok(()),
            Ok(lifecycle) => Err(StateError::Closed(lifecycle)),
            Err(()) => Err(StateError::Corrupt),
        }
    }

    pub(super) fn lifecycle(&self) -> Result<Lifecycle, StateError> {
        decode(self.state.load(Ordering::Acquire)).map_err(|()| StateError::Corrupt)
    }

    /// Classifies a missing scheduler Thread after stop publication.
    ///
    /// The Acquire load pairs with terminal/detach/reap publication. Only a
    /// state proving independent completion makes `ThreadNotFound` benign;
    /// an exact `StopRequested` endpoint still requires scheduler ownership.
    pub(super) fn thread_absence_is_terminal(&self) -> Result<bool, StateError> {
        self.lifecycle().map(|lifecycle| {
            matches!(
                lifecycle,
                Lifecycle::GuestTerminal(_) | Lifecycle::HardwareDetached(_) | Lifecycle::Reaped(_)
            )
        })
    }

    /// Publishes a guest-policy terminal result after hardware detachment.
    ///
    /// An administrative request which linearized first retains ownership of
    /// the stop. The runner then completes the administrative lifecycle rather
    /// than overwriting it with a later guest exit observation.
    pub(super) fn close_guest(
        &self,
        reason: TerminalReason,
    ) -> Result<GuestCloseOutcome, TransitionError> {
        match self.state.compare_exchange(
            STARTED,
            encode_terminal(reason),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(GuestCloseOutcome::Published),
            Err(observed) => match decode(observed) {
                Ok(Lifecycle::StopRequested(_)) => Ok(GuestCloseOutcome::AdministrativeStopPending),
                Ok(lifecycle) => Err(TransitionError::Unexpected(lifecycle)),
                Err(()) => Err(TransitionError::Corrupt),
            },
        }
    }

    pub(super) fn request_stop(
        &self,
        reason: AdministrativeStopReason,
    ) -> Result<StopRequestOutcome, StateError> {
        let mut observed = self.state.load(Ordering::Acquire);
        loop {
            match decode(observed) {
                Ok(Lifecycle::Dormant | Lifecycle::Started) => {
                    match self.state.compare_exchange_weak(
                        observed,
                        encode_stop_requested(reason),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return Ok(StopRequestOutcome::Published),
                        Err(next) => observed = next,
                    }
                }
                Ok(Lifecycle::StopRequested(_)) => {
                    return Ok(StopRequestOutcome::AlreadyRequested);
                }
                Ok(Lifecycle::GuestTerminal(reason)) => {
                    return Ok(StopRequestOutcome::GuestTerminal(reason));
                }
                Ok(Lifecycle::HardwareDetached(_)) => {
                    return Ok(StopRequestOutcome::HardwareDetached);
                }
                Ok(Lifecycle::Reaped(_)) => return Ok(StopRequestOutcome::Reaped),
                Ok(Lifecycle::Unbound) => return Ok(StopRequestOutcome::Inactive),
                Err(()) => return Err(StateError::Corrupt),
            }
        }
    }

    pub(super) fn publish_hardware_detached(
        &self,
        reason: AdministrativeStopReason,
    ) -> Result<(), TransitionError> {
        transition_exact(
            &self.state,
            encode_stop_requested(reason),
            encode_hardware_detached(reason),
        )
    }

    pub(super) fn publish_reaped(&self, reason: ClosureReason) -> Result<(), TransitionError> {
        let from = match reason {
            ClosureReason::Guest(reason) => encode_terminal(reason),
            ClosureReason::Administrative(reason) => encode_hardware_detached(reason),
        };
        transition_exact(&self.state, from, encode_reaped(reason))
    }
}

fn transition_exact(state: &AtomicU8, from: u8, to: u8) -> Result<(), TransitionError> {
    match state.compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(()),
        Err(observed) => match decode(observed) {
            Ok(lifecycle) => Err(TransitionError::Unexpected(lifecycle)),
            Err(()) => Err(TransitionError::Corrupt),
        },
    }
}

const fn encode_terminal(reason: TerminalReason) -> u8 {
    match reason {
        TerminalReason::MemoryFault => TERMINAL_MEMORY_FAULT,
        TerminalReason::Mmio => TERMINAL_MMIO,
        TerminalReason::Synchronous => TERMINAL_SYNCHRONOUS,
    }
}

const fn encode_stop_requested(_reason: AdministrativeStopReason) -> u8 {
    STOP_REQUESTED
}

const fn encode_hardware_detached(_reason: AdministrativeStopReason) -> u8 {
    HARDWARE_DETACHED
}

const fn encode_reaped(reason: ClosureReason) -> u8 {
    match reason {
        ClosureReason::Guest(TerminalReason::MemoryFault) => REAPED_GUEST_MEMORY_FAULT,
        ClosureReason::Guest(TerminalReason::Mmio) => REAPED_GUEST_MMIO,
        ClosureReason::Guest(TerminalReason::Synchronous) => REAPED_GUEST_SYNCHRONOUS,
        ClosureReason::Administrative(_) => REAPED_ADMINISTRATIVE,
    }
}

const fn decode(value: u8) -> Result<Lifecycle, ()> {
    match value {
        UNBOUND => Ok(Lifecycle::Unbound),
        DORMANT => Ok(Lifecycle::Dormant),
        STARTED => Ok(Lifecycle::Started),
        TERMINAL_MEMORY_FAULT => Ok(Lifecycle::GuestTerminal(TerminalReason::MemoryFault)),
        TERMINAL_MMIO => Ok(Lifecycle::GuestTerminal(TerminalReason::Mmio)),
        TERMINAL_SYNCHRONOUS => Ok(Lifecycle::GuestTerminal(TerminalReason::Synchronous)),
        STOP_REQUESTED => Ok(Lifecycle::StopRequested(
            AdministrativeStopReason::Requested,
        )),
        HARDWARE_DETACHED => Ok(Lifecycle::HardwareDetached(
            AdministrativeStopReason::Requested,
        )),
        REAPED_GUEST_MEMORY_FAULT => Ok(Lifecycle::Reaped(ClosureReason::Guest(
            TerminalReason::MemoryFault,
        ))),
        REAPED_GUEST_MMIO => Ok(Lifecycle::Reaped(ClosureReason::Guest(
            TerminalReason::Mmio,
        ))),
        REAPED_GUEST_SYNCHRONOUS => Ok(Lifecycle::Reaped(ClosureReason::Guest(
            TerminalReason::Synchronous,
        ))),
        REAPED_ADMINISTRATIVE => Ok(Lifecycle::Reaped(ClosureReason::Administrative(
            AdministrativeStopReason::Requested,
        ))),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dormant() -> EndpointState {
        let endpoint = EndpointState::unbound();
        assert_eq!(endpoint.publish_dormant(), Ok(()));
        endpoint
    }

    fn started() -> EndpointState {
        let endpoint = dormant();
        assert_eq!(endpoint.publish_started(), Ok(()));
        endpoint
    }

    #[test]
    fn administrative_lifecycle_is_one_way() {
        let endpoint = dormant();
        let reason = AdministrativeStopReason::Requested;
        assert_eq!(
            endpoint.request_stop(reason),
            Ok(StopRequestOutcome::Published)
        );
        assert_eq!(
            endpoint.request_stop(reason),
            Ok(StopRequestOutcome::AlreadyRequested)
        );
        assert_eq!(endpoint.publish_hardware_detached(reason), Ok(()));
        assert_eq!(
            endpoint.request_stop(reason),
            Ok(StopRequestOutcome::HardwareDetached)
        );
        let closure = ClosureReason::Administrative(reason);
        assert_eq!(endpoint.publish_reaped(closure), Ok(()));
        assert_eq!(
            endpoint.request_stop(reason),
            Ok(StopRequestOutcome::Reaped)
        );
        assert_eq!(endpoint.lifecycle(), Ok(Lifecycle::Reaped(closure)));
        assert_eq!(
            endpoint.ensure_live(),
            Err(StateError::Closed(Lifecycle::Reaped(closure)))
        );
    }

    #[test]
    fn guest_terminal_and_administrative_stop_have_distinct_ownership() {
        let terminal = started();
        assert_eq!(
            terminal.close_guest(TerminalReason::Mmio),
            Ok(GuestCloseOutcome::Published)
        );
        assert_eq!(
            terminal.request_stop(AdministrativeStopReason::Requested),
            Ok(StopRequestOutcome::GuestTerminal(TerminalReason::Mmio))
        );

        let stopped = started();
        assert_eq!(
            stopped.request_stop(AdministrativeStopReason::Requested),
            Ok(StopRequestOutcome::Published)
        );
        assert_eq!(
            stopped.close_guest(TerminalReason::Mmio),
            Ok(GuestCloseOutcome::AdministrativeStopPending)
        );
    }

    #[test]
    fn unbound_endpoint_is_explicitly_inactive() {
        let endpoint = EndpointState::unbound();
        assert_eq!(endpoint.lifecycle(), Ok(Lifecycle::Unbound));
        assert_eq!(
            endpoint.request_stop(AdministrativeStopReason::Requested),
            Ok(StopRequestOutcome::Inactive)
        );
        assert_eq!(endpoint.publish_dormant(), Ok(()));
        assert_eq!(endpoint.lifecycle(), Ok(Lifecycle::Dormant));
        assert_eq!(endpoint.publish_started(), Ok(()));
        assert_eq!(endpoint.lifecycle(), Ok(Lifecycle::Started));
    }

    #[test]
    fn dormant_and_started_are_distinct_one_way_states() {
        let endpoint = dormant();
        assert_eq!(endpoint.lifecycle(), Ok(Lifecycle::Dormant));
        assert_eq!(
            endpoint.close_guest(TerminalReason::Mmio),
            Err(TransitionError::Unexpected(Lifecycle::Dormant))
        );
        assert_eq!(endpoint.publish_started(), Ok(()));
        assert_eq!(endpoint.lifecycle(), Ok(Lifecycle::Started));
        assert_eq!(
            endpoint.publish_started(),
            Err(TransitionError::Unexpected(Lifecycle::Started))
        );
    }

    #[test]
    fn independently_started_endpoints_reject_duplicate_start_and_stop_races() {
        let boot = dormant();
        let secondary = dormant();

        assert_eq!(boot.publish_started(), Ok(()));
        assert_eq!(secondary.publish_started(), Ok(()));
        assert_eq!(boot.lifecycle(), Ok(Lifecycle::Started));
        assert_eq!(secondary.lifecycle(), Ok(Lifecycle::Started));
        assert_eq!(
            secondary.publish_started(),
            Err(TransitionError::Unexpected(Lifecycle::Started))
        );

        std::thread::scope(|scope| {
            let first = scope.spawn(|| secondary.request_stop(AdministrativeStopReason::Requested));
            let second =
                scope.spawn(|| secondary.request_stop(AdministrativeStopReason::Requested));
            let first = match first.join() {
                Ok(result) => result,
                Err(_) => panic!("first stop request panicked"),
            };
            let second = match second.join() {
                Ok(result) => result,
                Err(_) => panic!("second stop request panicked"),
            };
            assert!(matches!(
                (first, second),
                (
                    Ok(StopRequestOutcome::Published),
                    Ok(StopRequestOutcome::AlreadyRequested)
                ) | (
                    Ok(StopRequestOutcome::AlreadyRequested),
                    Ok(StopRequestOutcome::Published)
                )
            ));
        });

        assert_eq!(boot.lifecycle(), Ok(Lifecycle::Started));
        assert_eq!(
            secondary.lifecycle(),
            Ok(Lifecycle::StopRequested(
                AdministrativeStopReason::Requested
            ))
        );
    }

    #[test]
    fn guest_reap_preserves_the_exact_terminal_reason() {
        let endpoint = started();
        let closure = ClosureReason::Guest(TerminalReason::MemoryFault);
        assert_eq!(
            endpoint.close_guest(TerminalReason::MemoryFault),
            Ok(GuestCloseOutcome::Published)
        );
        assert_eq!(endpoint.publish_reaped(closure), Ok(()));
        assert_eq!(endpoint.lifecycle(), Ok(Lifecycle::Reaped(closure)));
    }

    #[test]
    fn invalid_and_skipped_transitions_do_not_mutate_state() {
        let endpoint = started();
        assert_eq!(
            endpoint.publish_reaped(ClosureReason::Administrative(
                AdministrativeStopReason::Requested
            )),
            Err(TransitionError::Unexpected(Lifecycle::Started))
        );
        assert_eq!(endpoint.lifecycle(), Ok(Lifecycle::Started));
    }

    #[test]
    fn guest_terminal_and_stop_request_have_one_atomic_winner() {
        let endpoint = started();
        std::thread::scope(|scope| {
            let guest = scope.spawn(|| endpoint.close_guest(TerminalReason::Synchronous));
            let stop = scope.spawn(|| endpoint.request_stop(AdministrativeStopReason::Requested));
            let guest = match guest.join() {
                Ok(result) => result,
                Err(_) => panic!("guest terminal publisher panicked"),
            };
            let stop = match stop.join() {
                Ok(result) => result,
                Err(_) => panic!("administrative stop publisher panicked"),
            };
            assert!(matches!(
                (guest, stop),
                (
                    Ok(GuestCloseOutcome::Published),
                    Ok(StopRequestOutcome::GuestTerminal(
                        TerminalReason::Synchronous
                    ))
                ) | (
                    Ok(GuestCloseOutcome::AdministrativeStopPending),
                    Ok(StopRequestOutcome::Published)
                )
            ));
        });
    }

    #[test]
    fn stop_published_while_irq_tail_is_suspended_blocks_reactivation() {
        use std::sync::atomic::{AtomicU8 as HostAtomicU8, Ordering as HostOrdering};

        let endpoint = started();
        let phase = HostAtomicU8::new(0);
        std::thread::scope(|scope| {
            let tail = scope.spawn(|| {
                // Model the interval after hardware detach and before the
                // scheduler resumes this exact Ready/Migrating continuation.
                phase.store(1, HostOrdering::Release);
                while phase.load(HostOrdering::Acquire) != 2 {
                    std::hint::spin_loop();
                }
                endpoint.lifecycle()
            });
            while phase.load(HostOrdering::Acquire) != 1 {
                std::hint::spin_loop();
            }
            assert_eq!(
                endpoint.request_stop(AdministrativeStopReason::Requested),
                Ok(StopRequestOutcome::Published)
            );
            phase.store(2, HostOrdering::Release);
            match tail.join() {
                Ok(state) => assert_eq!(
                    state,
                    Ok(Lifecycle::StopRequested(
                        AdministrativeStopReason::Requested
                    ))
                ),
                Err(_) => panic!("IRQ-tail continuation panicked"),
            }
        });
    }

    #[test]
    fn scheduler_prompt_loss_is_benign_only_after_terminal_progress() {
        let requested = dormant();
        assert_eq!(
            requested.request_stop(AdministrativeStopReason::Requested),
            Ok(StopRequestOutcome::Published)
        );
        assert_eq!(requested.thread_absence_is_terminal(), Ok(false));
        assert_eq!(
            requested.publish_hardware_detached(AdministrativeStopReason::Requested),
            Ok(())
        );
        assert_eq!(requested.thread_absence_is_terminal(), Ok(true));

        let guest = started();
        assert_eq!(
            guest.close_guest(TerminalReason::Synchronous),
            Ok(GuestCloseOutcome::Published)
        );
        assert_eq!(guest.thread_absence_is_terminal(), Ok(true));
    }
}
