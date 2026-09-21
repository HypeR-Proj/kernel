// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::{SwitchDisposition, SwitchHandoff};

#[test]
fn stale_and_replayed_tails_cannot_release_outgoing_context() {
    let mut handoff = SwitchHandoff::new();
    assert_eq!(handoff.begin(7u64, SwitchDisposition::Local), Some(1));
    assert!(handoff.complete(0).is_none());
    assert!(handoff.complete(2).is_none());
    assert_eq!(handoff.current().map(|context| context.thread), Some(7));
    assert_eq!(handoff.complete(1).map(|context| context.thread), Some(7));
    assert!(handoff.complete(1).is_none());
    assert_eq!(handoff.begin(9, SwitchDisposition::Coordinated), Some(2));
    assert!(handoff.complete(1).is_none());
    assert_eq!(handoff.current().map(|context| context.thread), Some(9));
    assert_eq!(handoff.count(), 2);
}

#[test]
fn occupied_handoff_rejects_new_switch_without_advancing_generation() {
    let mut handoff = SwitchHandoff::new();
    assert_eq!(handoff.begin(7u64, SwitchDisposition::Local), Some(1));
    assert_eq!(handoff.begin(9, SwitchDisposition::Coordinated), None);
    assert_eq!(handoff.count(), 1);
    assert_eq!(
        handoff.for_ticket(1).map(|context| context.disposition),
        Some(SwitchDisposition::Local)
    );
    assert!(handoff.complete(1).is_some());
    assert_eq!(handoff.begin(9, SwitchDisposition::Coordinated), Some(2));
}

#[test]
fn coordinator_fallback_observation_does_not_consume_handoff() {
    let mut handoff = SwitchHandoff::new();
    assert_eq!(handoff.begin(7u64, SwitchDisposition::Coordinated), Some(1));
    assert_eq!(
        handoff.for_ticket(1).map(|context| context.disposition),
        Some(SwitchDisposition::Coordinated)
    );
    // The local tail defers to the coordinator; the same ticket remains valid.
    assert_eq!(handoff.complete(1).map(|context| context.thread), Some(7));
    assert!(handoff.current().is_none());
}

#[test]
fn cpu_handoffs_have_independent_ticket_namespaces() {
    let mut first = SwitchHandoff::new();
    let mut second = SwitchHandoff::new();
    assert_eq!(first.begin(7u64, SwitchDisposition::Local), Some(1));
    assert_eq!(second.begin(9u64, SwitchDisposition::Coordinated), Some(1));
    assert_eq!(first.complete(1).map(|context| context.thread), Some(7));
    assert_eq!(second.current().map(|context| context.thread), Some(9));
    assert_eq!(first.begin(11, SwitchDisposition::Local), Some(2));
    assert_eq!(second.complete(1).map(|context| context.thread), Some(9));
}

#[test]
fn exhausted_generation_never_wraps_or_publishes_context() {
    let mut handoff = SwitchHandoff::new();
    handoff.next_generation = u64::MAX - 1;
    assert_eq!(
        handoff.begin(7u64, SwitchDisposition::Local),
        Some(u64::MAX - 1)
    );
    assert!(handoff.complete(u64::MAX - 1).is_some());
    assert_eq!(handoff.begin(9, SwitchDisposition::Coordinated), None);
    assert!(handoff.current().is_none());
    assert_eq!(handoff.next_generation, u64::MAX);
    assert_eq!(handoff.count(), 1);
}

#[test]
fn statistics_saturate_without_affecting_ticket_validation() {
    let mut handoff = SwitchHandoff::new();
    handoff.completed_preparations = u64::MAX;
    assert_eq!(handoff.begin(7u64, SwitchDisposition::Local), Some(1));
    assert_eq!(handoff.count(), u64::MAX);
    assert!(handoff.complete(1).is_some());
}
