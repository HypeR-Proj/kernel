// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Binding cursor coverage and bounded-stack teardown in scheduled context.

use alloc::string::String;

use super::{Error, FileRecord, Node, Record, Records, node, record_charge};
use crate::kernel::accounting::{ResourceDomain, ResourceError, ResourceKind, ResourceLimits};
use crate::kernel::vfs::namespace_test::TestError;
use hyper::mm::FallibleArc;

pub(crate) fn run(domain: &ResourceDomain) -> Result<(), TestError> {
    let baseline = domain.usage().committed(ResourceKind::KernelMemoryBytes);
    let live = node(FileRecord::try_new(1, domain)?, domain)?;
    let expired = node(FileRecord::try_new(2, domain)?, domain)?;
    let mut records = Records::new();
    records.insert(binding(&expired, 2, domain)?);
    // Begin with B in the examined list, then insert A in the unexamined list.
    // A prefix-first cursor must not miss B when it expires between batches.
    if !matches!(records.inspect_next(), Some(None)) {
        return Err(TestError::Contract("record cursor retains live entry"));
    }
    records.insert(binding(&live, 1, domain)?);
    drop(expired);
    for _ in 0..records.cleanup_bound() {
        drop(records.inspect_next());
    }
    if records.len() != 1 || records.iter().any(|record| record.id != 1) {
        return Err(TestError::Contract("record cursor reaches expired suffix"));
    }
    drop(records);
    let live_usage = domain.usage().committed(ResourceKind::KernelMemoryBytes);

    // An error after a large prepared prefix exercises unwinding both owning
    // chains. Recursive Box-tail destruction would exhaust a kernel stack.
    let failed = prepare_prefix_then_fail(&live, domain);
    if !matches!(failed, Err(Error::InvalidInput))
        || domain.usage().committed(ResourceKind::KernelMemoryBytes) != live_usage
    {
        return Err(TestError::Contract(
            "failed preparation releases all bindings",
        ));
    }
    drop(live);
    if domain.usage().committed(ResourceKind::KernelMemoryBytes) != baseline {
        return Err(TestError::Contract("record teardown restores accounting"));
    }
    quota_retirement()?;
    prediction_does_not_lease_node(domain)?;
    Ok(())
}

/// A running prediction may retain cached content after close, but must not
/// extend the active-node lifetime that namespace removal uses for Busy.
fn prediction_does_not_lease_node(domain: &ResourceDomain) -> Result<(), TestError> {
    let active = node(FileRecord::try_new(42, domain)?, domain)?;
    let weak = active.downgrade();
    let prediction = super::super::instance::ReadAheadFile::new(&active);
    let content = prediction
        .content_if_open()
        .ok_or(TestError::Contract("prediction retains live content"))?;
    drop(active);
    if weak.is_alive() || prediction.content_if_open().is_some() || content.id() != 42 {
        return Err(TestError::Contract(
            "prediction does not retain namespace lease",
        ));
    }
    Ok(())
}

/// Weak bindings continue to pay for both control blocks. A content owner
/// (for example a clean cache page) can retain identity after the final open;
/// reclamation may release quota only after that independent owner retires.
fn quota_retirement() -> Result<(), TestError> {
    let parent =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(Error::Resource)?;
    let domain = parent
        .try_new_child(ResourceLimits::UNLIMITED)
        .map_err(Error::Resource)?;
    let baseline = parent.usage().committed(ResourceKind::KernelMemoryBytes);
    let mut records = Records::new();
    let active = node(FileRecord::try_new(41, &domain)?, &domain)?;
    let content = active.record.clone();
    let weak = content.downgrade();
    records.insert(binding(&active, 41, &domain)?);
    drop(active);
    if !matches!(records.inspect_next(), Some(None)) || records.len() != 1 {
        return Err(TestError::Contract("cache content retains path binding"));
    }
    let held = parent.usage().committed(ResourceKind::KernelMemoryBytes);
    parent
        .set_local_limits(ResourceLimits::UNLIMITED.with(ResourceKind::KernelMemoryBytes, held))
        .map_err(Error::Resource)?;
    if !matches!(record_charge(&domain), Err(Error::Resource(ResourceError::LimitExceeded { domain: denied, .. })) if denied == parent.id())
    {
        return Err(TestError::Contract(
            "weak binding retention is charged to parent",
        ));
    }
    drop(content);
    if weak.upgrade().is_some() {
        return Err(TestError::Contract(
            "content owner expires after last release",
        ));
    }
    drop(weak);
    let retired = records.inspect_next();
    if !matches!(&retired, Some(Some(_))) || records.len() != 0 {
        return Err(TestError::Contract("expired record detaches from cursor"));
    }
    // The detached owner pays until dropped, including on caller error paths.
    drop(retired);
    if parent.usage().committed(ResourceKind::KernelMemoryBytes) != baseline {
        return Err(TestError::Contract("record retirement releases accounting"));
    }
    drop(record_charge(&domain)?);
    drop(domain);
    if parent.usage().committed(ResourceKind::KernelMemoryBytes) != 0 {
        return Err(TestError::Contract("record sponsor metadata retires"));
    }
    Ok(())
}

fn prepare_prefix_then_fail(
    owner: &FallibleArc<Node>,
    domain: &ResourceDomain,
) -> Result<Records, Error> {
    let mut records = Records::new();
    for id in 0..1024 {
        records.insert(binding(owner, id, domain)?);
    }
    for _ in 0..512 {
        drop(records.inspect_next());
    }
    Err(Error::InvalidInput)
}

fn binding(
    owner: &FallibleArc<Node>,
    id: u64,
    domain: &ResourceDomain,
) -> Result<super::PreparedRecord, Error> {
    Records::prepare(Record {
        id,
        path: String::new(),
        node: owner.downgrade(),
        content: owner.record.downgrade(),
        _charge: record_charge(domain)?,
    })
}
