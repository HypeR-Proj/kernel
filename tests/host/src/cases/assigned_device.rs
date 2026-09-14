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
