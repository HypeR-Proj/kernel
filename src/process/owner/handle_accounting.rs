// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Committed handle charges and their generation-indexed lookup.
//!
//! All operations borrow this owner under the existing Process state lock.
//! Returned charges/records/pages are released by the transaction after locks
//! are left. This owner adds neither a lock nor a Process lifecycle decision.

use super::{
    ChargeReservation, CommittedCharge, FallibleArc, HandleChargeLocation, HandleChargeRecord,
    HandleSidecar, HandleSidecarPlan, HandleValue, process_invariant_violation,
};

pub(super) struct HandleAccounting {
    records: Option<FallibleArc<HandleChargeRecord>>,
    index: HandleSidecar<HandleChargeLocation>,
}

impl HandleAccounting {
    pub(super) const fn new(index: HandleSidecar<HandleChargeLocation>) -> Self {
        Self {
            records: None,
            index,
        }
    }

    pub(super) fn install_storage(&mut self, plan: &mut HandleSidecarPlan<HandleChargeLocation>) {
        self.index.install(plan);
    }

    /// Keep backing destruction outside the Process lock and ahead of quota release.
    pub(super) fn detach_empty_page(&mut self, index: usize) -> impl Sized + use<> {
        self.index.detach_empty(index)
    }

    // Retirement deliberately detaches records before draining Thread owners,
    // and clears the index afterwards. Keep these phases independently callable.
    pub(super) fn take_records(&mut self) -> Option<FallibleArc<HandleChargeRecord>> {
        self.records.take()
    }

    pub(super) fn take_index(&mut self) -> HandleSidecar<HandleChargeLocation> {
        self.index.take()
    }

    pub(super) fn install(
        &mut self,
        record: FallibleArc<HandleChargeRecord>,
        charges: &mut alloc::vec::Vec<ChargeReservation>,
    ) {
        // Commit only pre-reserved charges after table publication. This cannot
        // allocate or fail; storage ownership stays with the caller until unlock.
        record.state.with(|state| {
            if state.entries.len() != charges.len() {
                process_invariant_violation();
            }
            for entry in state.entries.iter_mut().rev() {
                let charge = match charges.pop() {
                    Some(charge) => charge,
                    None => process_invariant_violation(),
                };
                entry.charge = Some(charge.commit());
            }
            if !charges.is_empty() {
                process_invariant_violation();
            }
        });
        record.state.with(|entries| {
            for (entry, charge) in entries.entries.iter().enumerate() {
                if self
                    .index
                    .replace(
                        charge.value,
                        Some(HandleChargeLocation {
                            record: record.clone(),
                            entry,
                        }),
                    )
                    .is_some()
                {
                    process_invariant_violation();
                }
            }
        });
        let previous = self.records.take();
        if let Some(previous) = previous.as_ref() {
            previous
                .previous
                .with(|link| *link = Some(FallibleArc::downgrade(&record)));
        }
        record.next.with(|link| *link = previous);
        self.records = Some(record);
    }

    pub(super) fn release(
        &mut self,
        value: HandleValue,
    ) -> (CommittedCharge, Option<FallibleArc<HandleChargeRecord>>) {
        let location = match self.index.replace(value, None) {
            Some(location) => location,
            None => process_invariant_violation(),
        };
        let record = location.record;
        let (charge, empty) = record.state.with(|state| {
            let entry = &mut state.entries[location.entry];
            if entry.value != value {
                process_invariant_violation();
            }
            let charge = match entry.charge.take() {
                Some(charge) => charge,
                None => process_invariant_violation(),
            };
            (
                charge,
                state.entries.iter().all(|entry| entry.charge.is_none()),
            )
        });
        if !empty {
            return (charge, None);
        }
        let previous = record.previous.with(Option::take);
        let next = record.next.with(Option::take);
        if let Some(next) = next.as_ref() {
            next.previous.with(|link| *link = previous.clone());
        }
        if let Some(previous) = previous {
            let previous = match previous.upgrade() {
                Some(previous) => previous,
                None => process_invariant_violation(),
            };
            previous.next.with(|link| *link = next);
        } else {
            self.records = next;
        }
        (charge, Some(record))
    }

    pub(super) fn is_live(&self, value: HandleValue) -> bool {
        self.index.get(value).is_some_and(|location| {
            location.record.state.with(|state| {
                let entry = &state.entries[location.entry];
                entry.value == value && entry.charge.is_some()
            })
        })
    }

    pub(super) fn replace(&mut self, previous: HandleValue, replacement: HandleValue) {
        let location = match self.index.replace(previous, None) {
            Some(location) => location,
            None => process_invariant_violation(),
        };
        location.record.state.with(|state| {
            let entry = &mut state.entries[location.entry];
            if entry.value != previous || entry.charge.is_none() {
                process_invariant_violation();
            }
            entry.value = replacement;
        });
        if self.index.replace(replacement, Some(location)).is_some() {
            process_invariant_violation();
        }
    }
}
