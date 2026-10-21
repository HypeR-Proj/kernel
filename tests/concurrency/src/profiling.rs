// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
use super::profile_admission::Admission;
use loom::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

#[test]
fn stop_recycle_never_overlaps_an_old_irq_writer() {
    loom::model(|| {
        let gate = Arc::new(Admission::<1>::new());
        let payload = Arc::new(AtomicU64::new(1));
        gate.publish(1);
        let irq_gate = gate.clone();
        let irq_payload = payload.clone();
        let irq = thread::spawn(move || {
            let requested = irq_gate.requested();
            if requested == 1 && irq_gate.claim(0, requested) {
                // Models a non-atomic buffer read/write admitted by the gate.
                assert_eq!(irq_payload.load(Ordering::Relaxed), 1);
                irq_payload.store(3, Ordering::Relaxed);
                irq_gate.release(0);
            }
        });
        gate.close(1);
        if gate.quiescent() {
            payload.store(2, Ordering::Relaxed);
            gate.publish(super::profile_admission::MAX_GENERATION);
        }
        super::require_ok(irq.join());
        assert_eq!(gate.owner(0), 0);
    });
}

#[test]
fn a_late_deadline_cannot_close_a_new_capture() {
    loom::model(|| {
        let gate = Arc::new(Admission::<1>::new());
        gate.publish(1);
        let other = gate.clone();
        let irq = thread::spawn(move || {
            other.close(1);
        });
        gate.close(1);
        gate.publish(super::profile_admission::MAX_GENERATION);
        super::require_ok(irq.join());
        assert_eq!(gate.requested(), super::profile_admission::MAX_GENERATION);
    });
}

#[test]
fn quiescence_acquires_every_cpu_writer_before_recycling() {
    loom::model(|| {
        let gate = Arc::new(Admission::<2>::new());
        let buffers = Arc::new([AtomicU64::new(0), AtomicU64::new(0)]);
        gate.publish(1);
        assert!(gate.claim(0, 1));
        assert!(gate.claim(1, 1));
        let mut writers = Vec::new();
        for cpu in 0..2 {
            let gate = gate.clone();
            let buffers = buffers.clone();
            writers.push(thread::spawn(move || {
                buffers[cpu].store(1, Ordering::Relaxed);
                gate.release(cpu);
            }));
        }
        gate.close(1);
        if gate.quiescent() {
            // Assert before join; only the production admission supplies the
            // ordering needed to safely reuse both non-atomic sample buffers.
            assert_eq!(buffers[0].load(Ordering::Relaxed), 1);
            assert_eq!(buffers[1].load(Ordering::Relaxed), 1);
        }
        for writer in writers {
            super::require_ok(writer.join());
        }
    });
}

#[test]
fn stop_during_ipi_start_retains_hardware_until_disarmed() {
    loom::model(|| {
        let gate = Arc::new(Admission::<1>::new());
        let enabled = Arc::new(AtomicU64::new(0));
        gate.publish(1);
        let control = {
            let gate = gate.clone();
            let enabled = enabled.clone();
            thread::spawn(move || {
                let request = gate.requested();
                if request != 0 && gate.claim(0, request) {
                    // The request can close between admission and the register
                    // write. Ownership must cover programming and disarming.
                    enabled.store(request, Ordering::Relaxed);
                    if gate.requested() != request {
                        enabled.store(0, Ordering::Relaxed);
                        gate.release(0);
                    }
                }
            })
        };
        gate.close(1);
        if gate.quiescent() {
            assert_eq!(enabled.load(Ordering::Relaxed), 0);
        }
        super::require_ok(control.join());
        // Models the following stop IPI on the same CPU, after local start.
        if gate.owner(0) != 0 {
            enabled.store(0, Ordering::Relaxed);
            gate.release(0);
        }
        assert!(gate.quiescent());
        assert_eq!(enabled.load(Ordering::Relaxed), 0);
    });
}

#[test]
fn delayed_start_prompt_cannot_claim_a_replaced_capture() {
    loom::model(|| {
        let gate = Arc::new(Admission::<1>::new());
        let period = Arc::new(AtomicU64::new(10));
        gate.publish(1);
        let recipient = {
            let gate = gate.clone();
            let period = period.clone();
            thread::spawn(move || {
                // A coalesced IPI reads the durable request, not an opcode
                // retained from its sender. Configuration is read after claim.
                let request = gate.requested();
                if request != 0 && gate.claim(0, request) {
                    assert_eq!(period.load(Ordering::Relaxed), request * 10);
                    gate.release(0);
                }
            })
        };
        gate.close(1);
        if gate.quiescent() {
            period.store(20, Ordering::Relaxed);
            gate.publish(2);
        }
        super::require_ok(recipient.join());
    });
}
