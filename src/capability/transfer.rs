// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Accounted capability ownership between process-local namespaces.

use crate::accounting::CommittedCharge;
use crate::object::ObjectRetirement;

use alloc::vec::Vec;

use super::handle::{InTransitHandleBatch, PreparedHandle};

/// Active capabilities retaining the sponsor of their backing storage.
#[must_use = "publish or explicitly release the in-transit capabilities"]
pub(crate) struct InTransitCapabilities {
    handles: Option<InTransitHandleBatch>,
    storage_charge: Option<CommittedCharge>,
}

impl InTransitCapabilities {
    pub(crate) fn new(handles: InTransitHandleBatch, storage_charge: CommittedCharge) -> Self {
        Self {
            handles: Some(handles),
            storage_charge: Some(storage_charge),
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self.handles.as_ref() {
            Some(handles) => handles.len(),
            None => super::invariant_violation(),
        }
    }

    pub(crate) fn release(self) {
        let mut retirement = ObjectRetirement::new();
        self.release_into(&mut retirement);
        retirement.drain();
    }

    pub(crate) fn release_into(mut self, retirement: &mut ObjectRetirement) {
        let handles = match self.handles.take() {
            Some(handles) => handles,
            None => super::invariant_violation(),
        };
        handles.release_into(retirement);
        drop(self.storage_charge.take());
    }

    fn into_parts(mut self) -> (InTransitHandleBatch, CommittedCharge) {
        let handles = match self.handles.take() {
            Some(handles) => handles,
            None => super::invariant_violation(),
        };
        let charge = match self.storage_charge.take() {
            Some(charge) => charge,
            None => super::invariant_violation(),
        };
        (handles, charge)
    }

    pub(crate) fn into_prepared_handles(self) -> (Vec<PreparedHandle>, CommittedCharge) {
        let (handles, charge) = self.into_parts();
        (handles.into_prepared_handles(), charge)
    }
}

impl Drop for InTransitCapabilities {
    fn drop(&mut self) {
        if self.handles.is_some() || self.storage_charge.is_some() {
            super::invariant_violation();
        }
    }
}
