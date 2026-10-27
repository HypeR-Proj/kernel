// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper_vm_models::arm::gic::quiesce::ActiveAccessQuiesce;
use hyper_vm_models::interrupt::VirtualCpuId;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;
use {
    hyper_vm_models::arm::gic::GicInterruptId, hyper_vm_models::arm::gic::InterruptGroup,
    hyper_vm_models::arm::gic::InterruptTrigger, hyper_vm_models::arm::gic::VirtualGicBuilder,
};

use crate::reconcile::ReconcilePublication;
use crate::require_ok;

#[test]
fn observing_a_publication_acquires_the_saved_model_change() {
    loom::model(|| {
        let publication = Arc::new(ReconcilePublication::new());
        let payload = Arc::new(AtomicUsize::new(0));
        let publisher = {
            let publication = publication.clone();
            let payload = payload.clone();
            thread::spawn(move || {
                payload.store(42, Ordering::Relaxed);
                publication.publish();
            })
        };
        if publication.pending() {
            // Check before join: observation must itself acquire the producer.
            assert_eq!(payload.load(Ordering::Relaxed), 42);
        }
        require_ok(publisher.join());
        assert!(publication.pending());
        assert!(publication.claim_before_admission());
        assert!(!publication.pending());
    });
}

#[test]
fn a_quiesce_request_racing_entry_keeps_its_detach_notification() {
    check_entry(false);
}

#[test]
fn failed_entry_releases_a_concurrent_quiesce_request() {
    check_entry(true);
}

fn check_entry(abandon: bool) {
    loom::model(move || {
        let entering = VirtualCpuId::new(0);
        let requester = VirtualCpuId::new(1);
        let mut gate = require_ok(ActiveAccessQuiesce::try_new(2));
        assert!(gate.try_enter(requester));
        let interrupt = require_ok(GicInterruptId::new(48).ok_or("invalid fixture interrupt"));
        let mut builder = require_ok(VirtualGicBuilder::new(2));
        require_ok(builder.configure(
            interrupt,
            entering,
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Level,
        ));
        let controller = require_ok(builder.finish(1));
        // This mutex stands for the real shared interrupt-controller lock;
        // neither endpoint publication nor the hardware load holds it.
        let state = Arc::new(Mutex::new((gate, controller)));
        let publication = Arc::new(ReconcilePublication::new());
        let producer = {
            let state = state.clone();
            let publication = publication.clone();
            thread::spawn(move || {
                let prompts = {
                    let mut state = require_ok(state.lock());
                    let (gate, controller) = &mut *state;
                    require_ok(gate.begin_deactivate(requester, interrupt.get(), None, true));
                    gate.bank_saved(requester, controller);
                    gate.take_prompts()
                };
                if prompts & 1 != 0 {
                    publication.publish();
                }
            })
        };

        let claimed = publication.claim_before_admission();
        let admitted = require_ok(state.lock()).0.try_enter(entering);
        if !admitted || abandon {
            if admitted {
                let mut state = require_ok(state.lock());
                let (gate, controller) = &mut *state;
                gate.bank_saved(entering, controller);
            }
            if claimed {
                publication.restore();
            }
        }
        // An admitted vCPU enters hardware without a second destructive
        // claim. The producer may publish before, during, or after this step.
        require_ok(producer.join());
        let mut state = require_ok(state.lock());
        let (gate, controller) = &mut *state;
        if admitted && !abandon {
            if gate.gate_closed() {
                assert!(publication.pending(), "resident bank lost its exit request");
            }
            // The IRQ tail detaches before the following activation claims
            // publication. No periodic scheduling event is needed for progress.
            gate.bank_saved(entering, controller);
        }
        assert!(!gate.gate_closed());
        assert_eq!(gate.take(requester), Some(Ok(None)));
        assert!(gate.try_enter(entering));
        gate.bank_saved(entering, controller);
    });
}
