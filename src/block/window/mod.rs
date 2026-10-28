// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! A queue's bounded, mount-local source window. Userspace only receives read
//! mappings. Generations authorize submissions, while leases prevent reuse.

use super::{BlockQueue, Lock, QueueError, plan};
mod state;
use core::sync::atomic::{AtomicBool, Ordering};
use hyper::mm::FallibleArc;
use state::State;
use {crate::accounting::CommittedCharge, crate::accounting::ResourceDomain};
use {
    crate::mm::user_space::DomainAccount, crate::mm::user_space::KernelPageBackend,
    crate::mm::user_space::VmoObject, crate::mm::user_space::WritableVmo,
};

type Memory = WritableVmo<KernelPageBackend, DomainAccount>;

impl From<state::Error> for QueueError {
    fn from(error: state::Error) -> Self {
        match error {
            state::Error::Busy => Self::Busy,
            state::Error::Invalid => Self::Invalid,
            state::Error::ResourceLimit => Self::ResourceLimit,
        }
    }
}

pub(crate) struct IoWindow {
    memory: Memory,
    state: Lock<State>,
    domain: ResourceDomain,
    _charge: CommittedCharge,
}

impl BlockQueue {
    /// Repeated exports refer to the same backing, including provider handoff.
    /// There is never a writable userspace handle to this storage.
    pub(crate) fn create_window(&self) -> Result<VmoObject, QueueError> {
        let existing = self.device.state.with(|device| {
            if device.failed || self.queue.closed.load(Ordering::Acquire) {
                return Err(QueueError::Device(
                    crate::fs::primitives::block::Error::Disconnected,
                ));
            }
            Ok(self.queue.window.with(|slot| slot.clone()))
        })?;
        let window = if let Some(window) = existing {
            window
        } else {
            let object = VmoObject::try_new_writable(plan::MAX_PAYLOAD as u64, &self.queue.domain)
                .map_err(|_| QueueError::NoMemory)?;
            let memory = object.writable_clone().ok_or(QueueError::Invalid)?;
            memory
                .populate(0, plan::MAX_PAYLOAD as u64)
                .map_err(|_| QueueError::NoMemory)?;
            let prepared = FallibleArc::try_new(IoWindow {
                memory,
                domain: self.queue.domain.clone(),
                _charge: super::queue::charge(
                    &self.queue.domain,
                    FallibleArc::<IoWindow>::allocation_size(),
                )?,
                state: Lock::new(State::new()),
            })
            .map_err(|_| QueueError::NoMemory)?;
            let mut prepared = Some(prepared);
            // Closure and device failure share the device condition lock. A
            // candidate prepared before either transition must not publish after
            // it. Keep candidate destruction outside both locks.
            let result = self.device.state.with(|device| {
                if device.failed || self.queue.closed.load(Ordering::Acquire) {
                    return Err(QueueError::Device(
                        crate::fs::primitives::block::Error::Disconnected,
                    ));
                }
                self.queue.window.with(|slot| {
                    if slot.is_none() {
                        *slot = prepared.take();
                    }
                    slot.clone().ok_or(QueueError::Invalid)
                })
            });
            drop(prepared);
            result?
        };
        VmoObject::from_read_only_native(window.memory.clone(), &self.queue.domain)
            .map_err(|_| QueueError::NoMemory)
    }

    pub(crate) fn bind_window(&self) -> Result<FallibleArc<WindowBinding>, QueueError> {
        let window = self
            .queue
            .window
            .with(|slot| slot.clone())
            .ok_or(QueueError::Invalid)?;
        let binding = FallibleArc::try_new(WindowBinding {
            _charge: super::queue::charge(
                &window.domain,
                FallibleArc::<WindowBinding>::allocation_size(),
            )?,
            window: window.clone(),
            claimed: AtomicBool::new(false),
            published: AtomicBool::new(false),
        })
        .map_err(|_| QueueError::NoMemory)?;
        window.state.with(State::bind)?;
        binding.claimed.store(true, Ordering::Relaxed);
        Ok(binding)
    }

    pub(super) fn window_lease(
        &self,
        token: u64,
        ranges: &[(usize, usize)],
    ) -> Result<WindowLease, QueueError> {
        let window = self
            .queue
            .window
            .with(|slot| slot.clone())
            .ok_or(QueueError::Invalid)?;
        window
            .state
            .with(|state| state.acquire_lease(token, ranges))?;
        Ok(WindowLease { window })
    }
}

/// Namespace publication seals this binding. Failed unpublished mounts roll
/// back; successful mounts never recycle their window for another provider.
pub(crate) struct WindowBinding {
    window: FallibleArc<IoWindow>,
    claimed: AtomicBool,
    published: AtomicBool,
    _charge: CommittedCharge,
}
impl WindowBinding {
    pub(crate) fn window(&self) -> &FallibleArc<IoWindow> {
        &self.window
    }
    pub(crate) fn publish(&self) {
        self.published.store(true, Ordering::Release);
    }
}
impl Drop for WindowBinding {
    fn drop(&mut self) {
        if self.claimed.load(Ordering::Relaxed) {
            self.window.state.with(|state| {
                state.release_binding(self.published.load(Ordering::Acquire));
            });
        }
    }
}

impl IoWindow {
    /// The caller has already resolved input faults before entering filesystem
    /// locks. Filling is private until publication; failed fills never authorize I/O.
    pub(crate) fn begin(
        window: &FallibleArc<Self>,
        length: usize,
        fill: impl FnOnce(&Memory) -> Result<(), QueueError>,
    ) -> Result<WindowWriteGuard, QueueError> {
        if length == 0 || length > plan::MAX_PAYLOAD {
            return Err(QueueError::Invalid);
        }
        window.state.with(State::begin_fill)?;
        let result = fill(&window.memory);
        let token = window
            .state
            .with(|state| state.publish_fill(length, result.is_ok()));
        result?;
        let token = token?;
        Ok(WindowWriteGuard {
            window: window.clone(),
            token,
            finished: false,
        })
    }
}

pub(crate) struct WindowWriteGuard {
    window: FallibleArc<IoWindow>,
    token: u64,
    finished: bool,
}
impl WindowWriteGuard {
    pub(crate) fn token(&self) -> u64 {
        self.token
    }

    /// A reply is not proof of completion. Outstanding leases retire this window
    /// instead of allowing later application data to overwrite an active source.
    pub(crate) fn finish(mut self) -> Result<(), QueueError> {
        let result = self.window.state.with(State::finish).map_err(Into::into);
        self.finished = true;
        result
    }
}
impl Drop for WindowWriteGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.window.state.with(State::retire);
        }
    }
}

pub(super) struct WindowLease {
    window: FallibleArc<IoWindow>,
}
impl WindowLease {
    pub(super) fn memory(&self) -> &Memory {
        &self.window.memory
    }
}
impl Drop for WindowLease {
    fn drop(&mut self) {
        if !self.window.state.with(State::release_lease) {
            crate::diagnostics::crash::fatal(format_args!("block window lease underflow"));
        }
    }
}
