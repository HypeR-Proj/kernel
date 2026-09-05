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
