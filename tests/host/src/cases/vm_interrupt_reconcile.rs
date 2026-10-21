// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[path = "../../../../src/kernel/vm/reconcile.rs"]
mod reconcile_model;

use std::sync::{Arc, Barrier};

use reconcile_model::ReconcilePublication;

#[test]
fn publication_coalesces_and_restoration_reopens_work() {
    let publication = ReconcilePublication::new();
    assert!(!publication.pending());

    publication.publish();
    publication.publish();
    assert!(publication.claim_before_admission());
    assert!(!publication.claim_before_admission());

    publication.restore();
    assert!(publication.pending());
    assert!(publication.claim_before_admission());
}

#[test]
fn a_racing_publish_is_claimed_or_remains_pending() {
    // Keep this isolated store/RMW check on real atomics: Loom 0.7.2 admits
    // invalid histories here (https://github.com/tokio-rs/loom/issues/254).
    // The entry/gate interleavings are modeled separately with Loom.
    const ITERATIONS: usize = 256;

    for _ in 0..ITERATIONS {
        let publication = Arc::new(ReconcilePublication::new());
        let start = Arc::new(Barrier::new(2));
        let producer_publication = publication.clone();
        let producer_start = start.clone();
        let producer = std::thread::spawn(move || {
            producer_start.wait();
            producer_publication.publish();
        });

        start.wait();
        let claimed = publication.claim_before_admission();
        assert!(producer.join().is_ok());
        assert!(claimed || publication.pending());
    }
}

#[test]
fn entry_preserves_a_quiesce_request_until_bank_detachment() {
    use hyper::vm::arm::gic::quiesce::ActiveAccessQuiesce;
    use hyper::vm::arm::gic::{
        GicInterruptId, InterruptGroup, InterruptTrigger, VirtualGicBuilder,
    };
    use hyper::vm::interrupt::VirtualCpuId;

    // Use production state machines without an unrelated scheduling event
    // to release residency. The request arrives after admission.
    let entering = VirtualCpuId::new(0);
    let requester = VirtualCpuId::new(1);
    let irq = crate::require_some(GicInterruptId::new(48));
    let mut builder = crate::require_ok(VirtualGicBuilder::new(2));
    crate::require_ok(builder.configure(
        irq,
        entering,
        0x80,
        InterruptGroup::Group1,
        InterruptTrigger::Level,
    ));
    let mut controller = crate::require_ok(builder.finish(1));
    let mut gate = crate::require_ok(ActiveAccessQuiesce::try_new(2));
    let publication = ReconcilePublication::new();
    let mut slots = [None];

    // activate() consumes older work before try_enter(), then reserves
    // residency and loads its initial bank with local IRQs masked.
    assert!(!publication.claim_before_admission());
    assert!(gate.try_enter(entering));
    assert!(gate.try_enter(requester));
    crate::require_ok(controller.refill_current(entering, &mut slots));

    crate::require_ok(gate.begin_deactivate(requester, irq.get(), None, true));
    gate.bank_saved(requester, &mut controller);
    assert_eq!(gate.take_prompts(), 3);
    publication.publish();
    // Entry must leave the publication intact: only a complete detach
    // releases the bank awaited by the remote transaction.
    assert!(gate.gate_closed());
    assert!(gate.take(requester).is_none());
    assert!(
        publication.pending(),
        "IRQ tail must still detach this bank"
    );

    crate::require_ok(controller.synchronize(entering, &slots));
    gate.bank_saved(entering, &mut controller);
    assert!(!gate.gate_closed());
    assert_eq!(gate.take(requester), Some(Ok(None)));
    // The following activation may consume the publication, because it
    // starts detached and rechecks admission before reserving hardware.
    assert!(publication.claim_before_admission());
    assert!(gate.try_enter(entering));
    gate.bank_saved(entering, &mut controller);
}
