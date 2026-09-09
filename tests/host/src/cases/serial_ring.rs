// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[path = "../../../../src/kernel/vm/serial_ring.rs"]
mod model;

#[test]
fn invalid_progress_does_not_release_unconsumed_slots() {
    let produced = 16;
    for candidate in [0, 7, 17, u64::MAX] {
        let accepted = model::accept_consumer(produced, 8, candidate);
        assert_eq!(accepted, 8);
        assert_eq!(model::writable_slot(produced, accepted, 8), None);
    }
    assert_eq!(
        model::writable_slot(produced, model::accept_consumer(produced, 8, 9), 8),
        Some(0)
    );
}

#[test]
fn full_wrap_and_exhaustion_preserve_bounds() {
    for produced in 0u64..1000 {
        for outstanding in 0..=8 {
            let consumed = produced.saturating_sub(outstanding);
            let slot = model::writable_slot(produced, consumed, 8);
            if produced - consumed == 8 {
                assert_eq!(slot, None);
            } else {
                assert_eq!(slot, Some((produced % 8) as usize));
            }
        }
    }
    assert_eq!(model::writable_slot(u64::MAX, u64::MAX, 8), None);
    assert_eq!(model::writable_slot(0, 1, 8), None);
    assert_eq!(model::writable_slot(1, 1, 0), None);
}
