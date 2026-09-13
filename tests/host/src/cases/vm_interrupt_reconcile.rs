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
    assert!(publication.take());
    assert!(!publication.take());

    publication.restore();
    assert!(publication.pending());
    assert!(publication.take());
}

#[test]
fn a_racing_publish_is_claimed_or_remains_pending() {
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
        let claimed = publication.take();
        assert!(producer.join().is_ok());
        assert!(claimed || publication.pending());
    }
}

#[test]
fn every_hardware_refill_boundary_publishes_cross_vcpu_delivery() {
    // Hardware entry is not executable in host tests. Guard the integration
    // boundaries where the pure GIC model may move a resident SPI to another
    // CPU; its state-transition behavior is covered by the vgic tests.
    let timer = include_str!("../../../../src/kernel/vm/timer.rs");
    for handler in [
        "handle_interrupt",
        "handle_maintenance_interrupt",
        "handle_source_recovery",
    ] {
        let function = crate::require_some(timer.split(&format!("fn {handler}(")).nth(1));
        let body = crate::require_some(function.split("\nfn ").next());
        assert!(
            body.contains("update_active_interrupts("),
            "{handler} must drain after hardware refill"
        );
    }
    let helper = crate::require_some(timer.split("fn update_active_interrupts(").nth(1));
    let helper = crate::require_some(helper.split("\nfn ").next());
    assert!(helper.contains("super::registry::with_binding"));
    assert!(helper.contains("VmBinding::publish_changed_interrupts"));

    let transition = include_str!("../../../../src/kernel/vm/vcpu/transition.rs");
    let activation = crate::require_some(transition.split("/// Removes active publication").next());
    let publication = crate::require_some(activation.rfind("active_vcpu::set_raw"));
    let drain = crate::require_some(activation.rfind("binding.publish_changed_interrupts();"));
    assert!(
        drain > publication,
        "successful activation must publish refill-generated prompts"
    );

    let registry = include_str!("../../../../src/kernel/vm/registry/execution.rs");
    let publication = crate::require_some(registry.split("fn publish_interrupt_reconcile(").nth(1));
    let publication = crate::require_some(publication.split("\n    pub(").next());
    assert!(
        !publication.contains("current_index() == Some(cpu)"),
        "a local drainer can consume a remote producer's dirty bit after its last refill"
    );
    assert!(publication.contains("request_guest_exit(cpu)"));
}
