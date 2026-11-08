// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Batch commit, rollback and stop interleavings using production delivery code.

use super::*;

pub(super) fn batch_delivery(domain: &ResourceDomain) -> Result<(), WaitSetError> {
    let set = WaitSet::try_new(3, domain)?;
    let make = |id| -> Result<_, WaitSetError> {
        let charge = domain
            .reserve(ResourceAmount::ZERO.with(
                ResourceKind::KernelMemoryBytes,
                FallibleArc::<Registration>::allocation_size() as u64,
            ))
            .map_err(WaitSetError::Resource)?
            .commit();
        FallibleArc::try_new(Registration {
            id,
            queue: set.queue.downgrade(),
            active: AtomicBool::new(true),
            pending: AtomicBool::new(false),
            _charge: charge,
        })
        .map_err(|_| WaitSetError::Allocation)
    };
    let registrations = [make(101)?, make(102)?, make(103)?];
    let source = SignalState::new();
    let mask = SignalMask::from_trusted_bits(1);
    source
        .update(SignalMask::EMPTY, mask)
        .map_err(|_| WaitSetError::InvalidInput)?;
    let snapshot = source.observe(mask).ok_or(WaitSetError::InvalidInput)?;
    for registration in &registrations {
        Registration::notify(registration, snapshot);
    }
    let usage = domain.usage().total(ResourceKind::KernelMemoryBytes);
    // A fault before any commit restores the original head and keeps all credits.
    let failed = set.read_batch(
        0,
        3,
        domain,
        || false,
        |_, _| Err(WaitSetError::Unsupported),
    );
    if !matches!(failed, Err(WaitSetError::Unsupported))
        || registrations.iter().any(|r| !r.pending())
    {
        return Err(WaitSetError::InvalidInput);
    }
    let copied = set.read_batch(
        0,
        3,
        domain,
        || false,
        |index, record| {
            if record[..8] != registrations[index].id.to_le_bytes()
                || !registrations[index].pending()
            {
                return Err(WaitSetError::InvalidInput);
            }
            if index == 1 {
                return Err(WaitSetError::Unsupported);
            }
            Ok(())
        },
    )?;
    if copied != 1 || registrations[0].pending() || !registrations[1].pending() {
        return Err(WaitSetError::InvalidInput);
    }
    // Another consumer can commit the next event while the outer consumer owns
    // the first. Each claim is unique, and the outer call returns a short batch.
    let mut nested_count = 0;
    let copied = set.read_batch(
        u64::MAX,
        3,
        domain,
        || false,
        |_, record| {
            if record[..8] != 102u64.to_le_bytes() {
                return Err(WaitSetError::InvalidInput);
            }
            nested_count = set.read_batch(
                0,
                1,
                domain,
                || false,
                |_, other| {
                    if other[..8] != 103u64.to_le_bytes() {
                        return Err(WaitSetError::InvalidInput);
                    }
                    Ok(())
                },
            )?;
            Ok(())
        },
    )?;
    if copied != 1
        || nested_count != 1
        || registrations.iter().any(|r| r.pending())
        || domain.usage().total(ResourceKind::KernelMemoryBytes) != usage
    {
        return Err(WaitSetError::InvalidInput);
    }
    // A stop after one successful copy preserves that prefix and leaves the
    // untouched second event available for another consumer.
    for r in &registrations[..2] {
        Registration::notify(r, snapshot);
    }
    let cancelled = AtomicBool::new(false);
    let copied = set.read_batch(
        0,
        3,
        domain,
        || cancelled.load(Ordering::Acquire),
        |_, _| {
            cancelled.store(true, Ordering::Release);
            Ok::<_, WaitSetError>(())
        },
    )?;
    if copied != 1 || !registrations[1].pending() {
        return Err(WaitSetError::InvalidInput);
    }
    let copied = set.read_batch(
        0,
        3,
        domain,
        || false,
        |_, record| {
            if record[..8] != 102u64.to_le_bytes() {
                return Err(WaitSetError::InvalidInput);
            }
            set.close();
            Ok::<_, WaitSetError>(())
        },
    )?;
    if copied != 1 || !matches!(set.try_take(), Err(WaitSetError::Closed)) {
        return Err(WaitSetError::InvalidInput);
    }
    Ok(())
}
