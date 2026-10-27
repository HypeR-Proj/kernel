// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free process and thread lifecycle state machines.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessPhase {
    Prepared,
    Created,
    Running,
    Stopping,
    Stopped,
    Retiring,
    Retired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalReason {
    Requested,
    /// Thread-local outcome supplied by an explicit or normal Thread exit.
    ThreadExited {
        status: i64,
    },
    /// Process-wide outcome propagated to every member Thread.
    ProcessExited {
        status: i64,
    },
    /// Process outcome synthesized when its final Thread detaches.
    LastThreadExited {
        status: i64,
    },
    Fault {
        class: u32,
        code: u64,
    },
    TaskGroupStop {
        generation: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LifecycleError {
    AdmissionClosed,
    AlreadyPublished,
    NotStarted,
    NotStopped,
    AlreadyRetired,
    CounterOverflow,
    InvalidMembership,
}

/// Tracks stop work which was admitted before publication or dispatched to an
/// already-published owner.
///
/// The initial count closes the reporting race where a stop request cannot yet
/// visit a prepared member. Each published member then contributes one more
/// incomplete operation only when its stop dispatch is not acknowledged.
pub(crate) struct StopDispatchProgress {
    incomplete: usize,
}

impl StopDispatchProgress {
    pub(crate) const fn new(pending: usize) -> Self {
        Self {
            incomplete: pending,
        }
    }

    pub(crate) fn observe(&mut self, complete: bool) {
        if !complete {
            self.incomplete = self.incomplete.saturating_add(1);
        }
    }

    pub(crate) const fn incomplete(&self) -> usize {
        self.incomplete
    }

    pub(crate) const fn is_complete(&self) -> bool {
        self.incomplete == 0
    }
}

/// Process-local lifecycle protected by the owning Process lock.
pub(crate) struct ProcessLifecycle {
    phase: ProcessPhase,
    pending_threads: usize,
    active_threads: usize,
    // An admitted creation can outlive the last active Thread. Preserve its
    // exit status until that creation either publishes or rolls back.
    last_thread_exit: Option<i64>,
    terminal: Option<TerminalReason>,
}

impl ProcessLifecycle {
    pub(crate) const fn prepared() -> Self {
        Self {
            phase: ProcessPhase::Prepared,
            pending_threads: 0,
            active_threads: 0,
            last_thread_exit: None,
            terminal: None,
        }
    }

    pub(crate) const fn phase(&self) -> ProcessPhase {
        self.phase
    }

    pub(crate) const fn terminal(&self) -> Option<TerminalReason> {
        self.terminal
    }

    pub(crate) const fn pending_threads(&self) -> usize {
        self.pending_threads
    }

    pub(crate) const fn active_threads(&self) -> usize {
        self.active_threads
    }

    pub(crate) fn publish(&mut self) -> Result<(), LifecycleError> {
        if self.phase != ProcessPhase::Prepared {
            return Err(LifecycleError::AlreadyPublished);
        }
        self.phase = ProcessPhase::Created;
        Ok(())
    }

    pub(crate) fn start(&mut self) -> Result<(), LifecycleError> {
        match self.phase {
            ProcessPhase::Created => {
                self.phase = ProcessPhase::Running;
                Ok(())
            }
            ProcessPhase::Running => Err(LifecycleError::AdmissionClosed),
            ProcessPhase::Prepared => Err(LifecycleError::NotStarted),
            _ => Err(LifecycleError::AdmissionClosed),
        }
    }

    pub(crate) fn reserve_thread(&mut self) -> Result<(), LifecycleError> {
        if !matches!(self.phase, ProcessPhase::Created | ProcessPhase::Running) {
            return Err(LifecycleError::AdmissionClosed);
        }
        self.reserve_thread_counter()
    }

    /// Reserves the one initial Thread while its Process is still unpublished.
    ///
    /// Only the child-start transaction may use this transition. Requiring an
    /// empty membership prevents a generic Thread-creation path from turning
    /// `Prepared` into a second externally usable admission phase.
    pub(crate) fn reserve_initial_thread(&mut self) -> Result<(), LifecycleError> {
        if self.phase != ProcessPhase::Prepared
            || self.pending_threads != 0
            || self.active_threads != 0
        {
            return Err(LifecycleError::AdmissionClosed);
        }
        self.reserve_thread_counter()
    }

    fn reserve_thread_counter(&mut self) -> Result<(), LifecycleError> {
        self.pending_threads = self
            .pending_threads
            .checked_add(1)
            .ok_or(LifecycleError::CounterOverflow)?;
        Ok(())
    }

    /// Cancels one unpublished admission and reports a new stopped edge.
    pub(crate) fn abort_thread(&mut self) -> Result<bool, LifecycleError> {
        let before = self.phase;
        self.pending_threads = self
            .pending_threads
            .checked_sub(1)
            .ok_or(LifecycleError::InvalidMembership)?;
        self.finish_stop_if_quiescent();
        Ok(before != ProcessPhase::Stopped && self.phase == ProcessPhase::Stopped)
    }

    pub(crate) fn publish_thread(&mut self) -> Result<(), LifecycleError> {
        let pending = self
            .pending_threads
            .checked_sub(1)
            .ok_or(LifecycleError::InvalidMembership)?;
        let active = self
            .active_threads
            .checked_add(1)
            .ok_or(LifecycleError::CounterOverflow)?;
        self.pending_threads = pending;
        self.active_threads = active;
        self.last_thread_exit = None;
        Ok(())
    }

    /// Latches one immutable terminal outcome and closes new admission.
    pub(crate) fn request_stop(&mut self, reason: TerminalReason) -> bool {
        if matches!(self.phase, ProcessPhase::Retiring | ProcessPhase::Retired) {
            return false;
        }
        let won = self.terminal.is_none();
        if won {
            self.terminal = Some(reason);
        }
        if matches!(
            self.phase,
            ProcessPhase::Prepared | ProcessPhase::Created | ProcessPhase::Running
        ) {
            self.phase = ProcessPhase::Stopping;
        }
        self.finish_stop_if_quiescent();
        won
    }

    pub(crate) fn detach_thread(&mut self, status: i64) -> Result<(), LifecycleError> {
        self.active_threads = self
            .active_threads
            .checked_sub(1)
            .ok_or(LifecycleError::InvalidMembership)?;
        if self.active_threads == 0 {
            self.last_thread_exit = Some(status);
        }
        self.finish_stop_if_quiescent();
        Ok(())
    }

    pub(crate) fn begin_retirement(&mut self) -> Result<(), LifecycleError> {
        match self.phase {
            ProcessPhase::Stopped => {
                self.phase = ProcessPhase::Retiring;
                Ok(())
            }
            ProcessPhase::Retiring => Ok(()),
            ProcessPhase::Retired => Err(LifecycleError::AlreadyRetired),
            _ => Err(LifecycleError::NotStopped),
        }
    }

    pub(crate) fn finish_retirement(&mut self) -> Result<(), LifecycleError> {
        if self.phase != ProcessPhase::Retiring {
            return Err(LifecycleError::NotStopped);
        }
        // Only a quiescent Stopped process can enter Retiring, and every
        // admission API is closed in both phases. The zero counters therefore
        // remain an internal invariant rather than a recoverable outcome.
        self.phase = ProcessPhase::Retired;
        Ok(())
    }

    fn finish_stop_if_quiescent(&mut self) {
        if self.pending_threads != 0 || self.active_threads != 0 {
            return;
        }
        // Failed initial construction must not terminate an otherwise unused
        // Process. Natural exit requires a previously detached active Thread.
        if matches!(self.phase, ProcessPhase::Created | ProcessPhase::Running)
            && let Some(status) = self.last_thread_exit
        {
            if self.terminal.is_none() {
                self.terminal = Some(TerminalReason::LastThreadExited { status });
            }
            self.phase = ProcessPhase::Stopping;
        }
        if self.phase == ProcessPhase::Stopping {
            self.phase = ProcessPhase::Stopped;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UserThreadPhase {
    Prepared,
    Dormant,
    Runnable,
    StopRequested,
    Detached,
}

pub(crate) struct UserThreadLifecycle {
    phase: UserThreadPhase,
    terminal: Option<TerminalReason>,
}

impl UserThreadLifecycle {
    pub(crate) const fn prepared() -> Self {
        Self {
            phase: UserThreadPhase::Prepared,
            terminal: None,
        }
    }

    pub(crate) const fn phase(&self) -> UserThreadPhase {
        self.phase
    }

    pub(crate) const fn terminal(&self) -> Option<TerminalReason> {
        self.terminal
    }

    pub(crate) fn publish(&mut self) -> Result<(), LifecycleError> {
        if self.phase != UserThreadPhase::Prepared {
            return Err(LifecycleError::AlreadyPublished);
        }
        self.phase = UserThreadPhase::Dormant;
        Ok(())
    }

    pub(crate) fn mark_runnable(&mut self) -> Result<(), LifecycleError> {
        match self.phase {
            UserThreadPhase::Dormant => {
                self.phase = UserThreadPhase::Runnable;
                Ok(())
            }
            UserThreadPhase::Runnable => Ok(()),
            _ => Err(LifecycleError::AdmissionClosed),
        }
    }

    pub(crate) fn request_terminal(&mut self, reason: TerminalReason) -> bool {
        if self.phase == UserThreadPhase::Detached {
            return false;
        }
        let won = self.terminal.is_none();
        if won {
            self.terminal = Some(reason);
        }
        self.phase = UserThreadPhase::StopRequested;
        won
    }

    pub(crate) fn detach(&mut self) -> Result<TerminalReason, LifecycleError> {
        if self.phase == UserThreadPhase::Detached {
            return Err(LifecycleError::InvalidMembership);
        }
        let terminal = self
            .terminal
            .unwrap_or(TerminalReason::ThreadExited { status: 0 });
        self.terminal = Some(terminal);
        self.phase = UserThreadPhase::Detached;
        Ok(terminal)
    }
}
