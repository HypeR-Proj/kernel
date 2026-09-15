// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[path = "../../../../src/kernel/device/assigned/model.rs"]
mod model;

#[test]
fn assigned_dma_activation_requires_renegotiation_after_reset() {
    let mut state = model::Negotiation::new();
    assert!(!state.write(0x70, 15));
    assert!(state.write(0x20, 2));
    assert!(!state.write(0x70, 15)); // Low feature word cannot authorize bit 33.
    assert!(state.write(0x24, 1));
    assert!(state.write(0x20, 2));
    assert!(state.write(0x70, 15));
    assert!(state.write(0x70, 0));
    assert!(!state.write(0x70, 15));
    assert!(state.write(0x24, 1));
    assert!(state.write(0x20, 2));
    assert!(state.write(0x20, 0));
    assert!(!state.write(0x70, 15));
}

#[test]
fn late_physical_interrupt_after_guest_ack_leaves_next_completion_enabled() {
    use hyper::hal::interrupt::InterruptTrigger::Level;
    assert!(model::mask_interrupt(true, 1, Level));
    // Guest ACK cleared status and re-enabled the line before a latched IRQ
    // reached dispatch. There will be no further guest ACK for this empty IRQ.
    assert!(!model::mask_interrupt(true, 0, Level));
    assert!(model::mask_interrupt(true, 2, Level));
    assert!(model::mask_interrupt(false, 0, Level));
}

#[test]
fn qemu_edge_source_stays_enabled_across_coalesced_completions_and_ack() {
    use hyper::hal::interrupt::InterruptTrigger::{Edge, Level};
    // QEMU advertises an edge SPI even though virtio InterruptStatus is durable.
    // A guest ACK can race a queued physical edge. Neither nonempty status nor
    // a stale empty edge may leave this source waiting for another guest ACK.
    for status in [1, 3, 2, 0, 1, 0] {
        assert!(!model::mask_interrupt(true, status, Edge));
    }
    assert!(model::mask_interrupt(true, 1, Level));
    assert!(model::mask_interrupt(false, 1, Edge));
    assert!(model::mask_interrupt(false, 0, Edge));
}

#[test]
fn stale_irq_during_install_uses_assignment_state_not_registry_visibility() {
    use hyper::hal::interrupt::InterruptTrigger::{Edge, Level};
    // A reset device can retain a latched controller interrupt. Its handler
    // becomes active before VM registry publication, and no guest can ACK it
    // yet. Both empty level sources and edge sources must remain enabled.
    for trigger in [Edge, Level] {
        assert!(!model::mask_interrupt(true, 0, trigger));
        // Reset changes assignment state before unregister; an intervening
        // callback must now mask even though registry lookup also fails here.
        assert!(model::mask_interrupt(false, 0, trigger));
    }
}

#[test]
fn dma_extent_rejects_holes_discontiguity_and_overflow() {
    use model::{ExtentError, contiguous_extent};
    assert_eq!(
        contiguous_extent(4096, 8192, 16384, 4096, |offset| Some(0x8000 + offset)),
        Ok(0x9000)
    );
    assert_eq!(
        contiguous_extent(0, 8192, 8192, 4096, |offset| if offset == 0 {
            Some(0x8000)
        } else {
            None
        }),
        Err(ExtentError::Nonresident)
    );
    assert_eq!(
        contiguous_extent(0, 8192, 8192, 4096, |offset| Some(0x8000 + offset * 2)),
        Err(ExtentError::Noncontiguous)
    );
    assert_eq!(
        contiguous_extent(0, 8192, 8192, 4096, |_| Some(u64::MAX - 4095)),
        Err(ExtentError::Range)
    );
    assert_eq!(
        contiguous_extent(0, 8192, 4096, 4096, |_| Some(0x8000)),
        Err(ExtentError::Range)
    );
    assert_eq!(
        contiguous_extent(1, 4096, 8192, 4096, |_| Some(0x8000)),
        Err(ExtentError::Range)
    );
}

#[test]
fn firmware_selection_requires_exactly_one_match() {
    assert_eq!(model::unique_match([false, true, false]), Ok(1));
    assert_eq!(
        model::unique_match([false, false]),
        Err(model::SelectionError::Missing)
    );
    assert_eq!(
        model::unique_match([true, true]),
        Err(model::SelectionError::Ambiguous)
    );
    assert_eq!(model::unique_match([]), Err(model::SelectionError::Missing));
}

#[test]
fn physical_register_windows_reject_gaps_and_crossing_accesses() {
    assert_eq!(model::register_offset(0x3c00, 1, 0x3c00, 0x40), Some(0));
    assert_eq!(model::register_offset(0x3c3e, 2, 0x3c00, 0x40), Some(0x3e));
    assert_eq!(model::register_offset(0x3c3c, 4, 0x3c00, 0x40), Some(0x3c));
    for (address, width) in [
        (0x3000, 4),
        (0x3bfc, 4),
        (0x3c40, 1),
        (0x3c3f, 2),
        (0x3c00, 8),
        (usize::MAX, 4),
    ] {
        assert!(model::register_offset(address, width, 0x3c00, 0x40).is_none());
    }
    assert!(model::assignment_aperture(0x0b00_0000));
    assert!(model::assignment_aperture(0x0bff_0000));
    assert!(!model::assignment_aperture(0x0bff_f000));
    assert!(!model::assignment_aperture(u64::MAX));
}

#[test]
fn level_irq_observation_consumes_readiness_without_rearming_an_asserted_device() {
    use model::{LevelInterrupt, RearmDecision};
    let mut irq = LevelInterrupt::new();
    assert_eq!(irq.complete(0, true), Ok(RearmDecision::NoRearm));
    assert!(!irq.readable());
    assert_eq!(irq.deliver(), Ok(()));
    let token = irq.pending();
    assert_ne!(token, 0);
    assert!(irq.readable());
    assert_eq!(irq.complete(token, true), Ok(RearmDecision::NoRearm));
    assert_eq!(irq.pending(), token);
    assert!(!irq.readable());
    assert!(!irq.can_rearm());
    assert_eq!(irq.complete(token, false), Ok(RearmDecision::Rearm));
    assert_eq!(irq.pending(), 0);
    assert!(!irq.readable());
    assert!(irq.can_rearm());
}

#[test]
fn new_level_irq_between_ack_and_rearm_prevents_late_unmask_or_stale_ack() {
    use model::{LevelInterrupt, RearmDecision, StaleSequence};
    let mut irq = LevelInterrupt::new();
    assert_eq!(irq.deliver(), Ok(()));
    let old = irq.pending();
    assert_eq!(irq.complete(old, false), Ok(RearmDecision::Rearm));
    // A delayed physical callback wins before the rearm predicate gets the
    // registry lock. Its pending token must survive the older syscall.
    assert_eq!(irq.deliver(), Ok(()));
    let new = irq.pending();
    assert!(new > old);
    assert!(!irq.can_rearm());
    let before = irq;
    assert_eq!(irq.complete(old, false), Err(StaleSequence));
    assert_eq!(irq.complete(0, true), Err(StaleSequence));
    assert_eq!(irq, before);
    assert_eq!(irq.complete(new, false), Ok(RearmDecision::Rearm));
}

#[test]
fn repeated_physical_delivery_coalesces_without_reusing_a_retired_token() {
    use model::{LevelInterrupt, RearmDecision};
    let mut irq = LevelInterrupt::new();
    assert_eq!(irq.complete(0, false), Ok(RearmDecision::Rearm));
    assert_eq!(irq.deliver(), Ok(()));
    let first = irq.pending();
    assert_eq!(irq.complete(first, true), Ok(RearmDecision::NoRearm));
    assert_eq!(irq.deliver(), Ok(()));
    assert_eq!(irq.pending(), first);
    assert!(irq.readable());
    assert_eq!(irq.complete(first, false), Ok(RearmDecision::Rearm));
    assert_eq!(irq.deliver(), Ok(()));
    assert!(irq.pending() > first);
}

#[test]
fn userspace_physical_mmio_requires_exact_owned_aperture_and_generic_profile() {
    use model::owns_userspace_aperture;
    let base = 0x0b00_0000;
    assert!(owns_userspace_aperture(true, base, base, 65536));
    assert!(!owns_userspace_aperture(false, base, base, 65536));
    assert!(!owns_userspace_aperture(true, base, base + 65536, 65536));
    assert!(!owns_userspace_aperture(true, base, base + 4096, 4096));
    assert!(!owns_userspace_aperture(true, base, base, 0));
    assert!(!owns_userspace_aperture(true, base, base, 4096));
    assert!(!owns_userspace_aperture(true, base, base, 131072));
    assert!(!owns_userspace_aperture(
        true,
        u64::MAX - 65535,
        u64::MAX - 65535,
        65536
    ));
}
