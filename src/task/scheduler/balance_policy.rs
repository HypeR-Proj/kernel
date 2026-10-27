// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Small placement rules, independent of scheduler synchronization.

/// Chooses the least runnable CPU, retaining the preferred CPU on equal load.
/// Candidate order resolves ties when the preferred CPU is unavailable.
pub(crate) fn least_loaded(
    preferred: usize,
    candidates: impl Iterator<Item = (usize, u32)>,
) -> Option<usize> {
    candidates
        .min_by_key(|(cpu, load)| (*load, *cpu != preferred))
        .map(|(cpu, _)| cpu)
}

/// One transfer must reduce the difference rather than exchange its sign.
pub(crate) const fn improves_balance(source: u32, target: u32) -> bool {
    source.saturating_sub(target) >= 2
}
