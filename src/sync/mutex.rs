// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! FIFO sleeping mutex with direct ownership handoff.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

use hyper::sync::InterruptSpinLock;

use super::Error;
use crate::task::scheduler;
use crate::task::thread::ThreadId;
use {crate::task::WaitMobility, crate::task::WaitQueue};

type StateLock = InterruptSpinLock<State, crate::hal::irq::LocalMask>;

struct State {
    owner: Option<ThreadId>,
    waiters: WaitQueue,
}

/// A thread-context mutex. Contended acquisition blocks instead of spinning.
pub struct Mutex<T: ?Sized> {
    state: StateLock,
    value: UnsafeCell<T>,
}

pub struct MutexGuard<'mutex, T: ?Sized> {
    mutex: &'mutex Mutex<T>,
    // A guard must be released by the thread that acquired it.
    not_send: PhantomData<*mut ()>,
}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: StateLock::new(State {
                owner: None,
                waiters: WaitQueue::new(),
            }),
            value: UnsafeCell::new(value),
        }
    }

    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    pub fn lock(&self) -> Result<MutexGuard<'_, T>, Error> {
        scheduler::ensure_sleepable()?;
        let current = scheduler::current_thread_id()?;
        // SAFETY: The retained mask is consumed into the saved machine context
        // at the final park boundary or dropped before this function proceeds.
        let (park, interrupt_mask) = unsafe {
            self.state.with_mask_retained(|state| match state.owner {
                None => {
                    state.owner = Some(current);
                    Ok(None)
                }
                Some(owner) if owner == current => Err(Error::WouldDeadlock),
                Some(_) => {
                    let registration = scheduler::begin_wait(WaitMobility::Migratable)?;
                    scheduler::prepare_registered_park_locked(&state.waiters, registration)
                        .map(Some)
                        .map_err(Error::from)
                }
            })
        };
        let park = park?;
        let Some(prepared) = park else {
            drop(interrupt_mask);
            return Ok(MutexGuard {
                mutex: self,
                not_send: PhantomData,
            });
        };
        let outcome = prepared.retain_mask(interrupt_mask).complete();
        super::expect_notification(outcome)?;
        Ok(MutexGuard {
            mutex: self,
            not_send: PhantomData,
        })
    }

    pub fn try_lock(&self) -> Result<Option<MutexGuard<'_, T>>, Error> {
        scheduler::ensure_sleepable()?;
        let current = scheduler::current_thread_id()?;
        self.state.with(|state| match state.owner {
            None => {
                state.owner = Some(current);
                Ok(Some(MutexGuard {
                    mutex: self,
                    not_send: PhantomData,
                }))
            }
            Some(owner) if owner == current => Err(Error::WouldDeadlock),
            Some(_) => Ok(None),
        })
    }

    pub fn is_locked(&self) -> bool {
        self.state.with(|state| state.owner.is_some())
    }

    pub fn waiter_count(&self) -> Result<usize, Error> {
        self.state
            .with(|state| state.waiters.len().map_err(Error::from))
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    fn unlock(&self) -> Result<(), Error> {
        let current = scheduler::current_thread_id()?;
        self.state.with(|state| {
            if state.owner != Some(current) {
                return Err(Error::NotOwner);
            }
            let waiters = &state.waiters;
            let owner = &mut state.owner;
            if scheduler::wake_one_with(waiters, |next| *owner = Some(next))?.is_none() {
                *owner = None;
            }
            Ok(())
        })
    }
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: The scheduler assigns one owner and transfers ownership
        // directly to exactly one FIFO waiter before it becomes runnable.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: A MutexGuard is unique to the current owner.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        if self.mutex.unlock().is_err() {
            // Drop can run beneath arbitrary subsystem locks. Ordinary logs may
            // deadlock while mutex ownership is inconsistent, so fail closed
            // without acquiring another lock.
            hyper::debug::invariant_failure("sync::mutex::drop invariant")
        }
    }
}

// SAFETY: Shared access to T is possible only through the unique owner guard.
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}
