// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Reversible namespace publication. The capability owner chooses commit;
//! owner loss restores the old immutable view without allocation or backend I/O.

use hyper::mm::FallibleArc;
use hyper::sync::SpinLock;

use super::Error;
use super::instance::MountNamespace;
use super::mounts::View;

struct State {
    old: Option<View>,
    prepared: Option<View>,
    active: bool,
}

pub(crate) struct RootTransition {
    namespace: FallibleArc<MountNamespace>,
    epoch: u64,
    state: SpinLock<State>,
}

impl RootTransition {
    pub(super) fn new(
        namespace: FallibleArc<MountNamespace>,
        old: View,
        prepared: View,
        epoch: u64,
    ) -> Self {
        Self {
            namespace,
            epoch,
            state: SpinLock::new(State {
                old: Some(old),
                prepared: Some(prepared),
                active: false,
            }),
        }
    }

    /// All object and handle storage must be reserved before activation. Any
    /// publication failure must call abort before returning to the caller.
    pub(crate) fn activate(&self) -> Result<(), Error> {
        let filesystem = self
            .state
            .with(|state| {
                state
                    .prepared
                    .as_ref()
                    .map(|view| view.root().mount().filesystem_owner().clone())
            })
            .ok_or(Error::InvalidInput)?;
        // Acquire the filesystem's sleeping lock before either spinlock.
        let retired = filesystem.with_namespace_stable(self.epoch, || {
            self.state.with(|state| {
                let old = state
                    .old
                    .as_ref()
                    .ok_or(super::instance::Error::InvalidInput)?;
                let retired = self
                    .namespace
                    .mounts
                    .activate_root_transition(old, &mut state.prepared)?;
                state.active = true;
                Ok(retired)
            })
        })?;
        drop(retired);
        Ok(())
    }

    pub(crate) fn finish(&self, commit: bool) -> Result<(), Error> {
        let retired = self.state.with(|state| {
            if !state.active {
                return Err(Error::InvalidInput);
            }
            let old = state.old.take().ok_or(Error::InvalidInput)?;
            state.active = false;
            Ok(self.namespace.mounts.finish_root_transition(old, commit))
        })?;
        drop(retired);
        Ok(())
    }

    /// Idempotent cleanup for rejected publication and last-active-handle loss.
    /// Retired topology and archive owners drop after the namespace locks.
    pub(crate) fn abort(&self) {
        let retired = self.state.with(|state| {
            let old = state.old.take();
            let prepared = state.prepared.take();
            let old = if state.active {
                state.active = false;
                old.map(|old| self.namespace.mounts.finish_root_transition(old, false))
            } else {
                old
            };
            (old, prepared)
        });
        drop(retired);
    }
}

impl Drop for RootTransition {
    fn drop(&mut self) {
        self.abort();
    }
}
