// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! SBSA FIFO ordering, backpressure, interrupt latches, and register boundaries.

use crate::require_ok;
use {
    hyper_vm_models::aarch64::device::sbsa_uart::AccessError,
    hyper_vm_models::aarch64::device::sbsa_uart::SbsaUart,
    hyper_vm_models::aarch64::device::sbsa_uart::reg,
};

const TIMEOUT_NS: u64 = 277_778;

#[test]
fn transmit_fifo_retains_output_until_the_transport_accepts_it() {
    let mut uart = SbsaUart::new();
    assert_eq!(read(&mut uart, reg::FR), reg::FR_RXFE | reg::FR_TXFE);
    write(&mut uart, reg::IMSC, reg::INT_TX, 0);
    assert!(!uart.interrupt_asserted());
    for byte in 0..32 {
        write(&mut uart, reg::DR, byte, 0);
    }
    assert!(!uart.can_transmit());
    assert_eq!(
        read(&mut uart, reg::FR),
        reg::FR_RXFE | reg::FR_TXFF | reg::FR_BUSY
    );
    assert_eq!(
        uart.write(reg::DR, 4, 99, 0),
        Err(AccessError::TransmitFull)
    );
    assert!(!uart.interrupt_asserted());

    for byte in 0..27 {
        assert_eq!(uart.pending_transmit(), Some(byte));
        assert_eq!(uart.pending_transmit(), Some(byte));
        assert_eq!(uart.take_transmitted(), Some(byte));
        assert!(!uart.interrupt_asserted());
    }
    assert_eq!(uart.take_transmitted(), Some(27)); // Five -> four crosses trigger.
    assert!(uart.can_transmit());
    assert!(uart.interrupt_asserted());
    write(&mut uart, reg::ICR, reg::INT_TX, 0);
    assert_eq!(uart.take_transmitted(), Some(28));
    assert!(!uart.interrupt_asserted());
    for byte in 29..32 {
        assert_eq!(uart.take_transmitted(), Some(byte));
    }
    assert!(uart.interrupt_asserted());
    assert_eq!(read(&mut uart, reg::FR), reg::FR_RXFE | reg::FR_TXFE);
    assert_eq!(uart.take_transmitted(), None);
}

#[test]
fn short_transmit_bursts_generate_one_empty_event_and_refill_clears_threshold_irq() {
    let mut uart = SbsaUart::new();
    write(&mut uart, reg::DR, u32::from(b'x'), 0);
    assert_eq!(read(&mut uart, reg::RIS), 0);
    assert_eq!(uart.take_transmitted(), Some(b'x'));
    assert_eq!(read(&mut uart, reg::RIS), reg::INT_TX);
    assert!(!uart.interrupt_asserted());
    write(&mut uart, reg::IMSC, reg::INT_TX, 0);
    assert!(uart.interrupt_asserted());
    write(&mut uart, reg::ICR, reg::INT_TX, 0);
    uart.advance(TIMEOUT_NS * 2);
    assert!(!uart.interrupt_asserted());
    assert_eq!(uart.take_transmitted(), None);
    assert_eq!(read(&mut uart, reg::RIS), 0);

    write(&mut uart, reg::DR, 0, 0);
    assert_eq!(uart.take_transmitted(), Some(0));
    for byte in 0..4 {
        write(&mut uart, reg::DR, byte, 0);
        assert!(uart.interrupt_asserted());
    }
    write(&mut uart, reg::DR, 4, 0);
    assert!(!uart.interrupt_asserted());
}

#[test]
fn receive_irq_requires_a_new_threshold_crossing_after_icr() {
    let mut uart = SbsaUart::new();
    write(&mut uart, reg::IMSC, reg::INT_RX, 0);
    for byte in 0..3 {
        assert!(uart.receive(byte, 0));
        assert!(!uart.interrupt_asserted());
    }
    assert!(uart.receive(3, 0));
    assert!(uart.interrupt_asserted());
    write(&mut uart, reg::ICR, reg::INT_RX, 0);
    assert!(!uart.interrupt_asserted());
    assert!(uart.receive(4, 0));
    assert_eq!(read(&mut uart, reg::DR), 0);
    assert!(!uart.interrupt_asserted());
    assert_eq!(read(&mut uart, reg::DR), 1);
    assert!(uart.receive(5, 0));
    assert!(uart.interrupt_asserted());
    assert_eq!(read(&mut uart, reg::DR), 2);
    assert!(!uart.interrupt_asserted());
}

#[test]
fn sparse_receive_timeout_is_32_bits_and_partial_reads_do_not_clear_it() {
    let mut uart = SbsaUart::new();
    write(&mut uart, reg::IMSC, reg::INT_RT, 0);
    assert!(uart.receive(b'a', 100));
    assert_eq!(uart.next_timeout(), Some(100 + TIMEOUT_NS));
    uart.advance(99 + TIMEOUT_NS);
    assert!(!uart.interrupt_asserted());
    assert!(uart.receive(b'b', 100 + TIMEOUT_NS - 1));
    uart.advance(100 + TIMEOUT_NS); // Stale callback from the first byte.
    assert!(!uart.interrupt_asserted());
    uart.advance(99 + TIMEOUT_NS * 2);
    assert!(uart.interrupt_asserted());
    assert_eq!(uart.next_timeout(), None);
    assert_eq!(read(&mut uart, reg::DR), u32::from(b'a'));
    assert!(uart.interrupt_asserted());
    assert!(uart.receive(b'c', 100 + TIMEOUT_NS * 2));
    assert!(uart.interrupt_asserted());
    assert_eq!(uart.next_timeout(), None);

    write(&mut uart, reg::ICR, reg::INT_RT, 200 + TIMEOUT_NS * 2);
    assert!(!uart.interrupt_asserted());
    assert_eq!(uart.next_timeout(), Some(200 + TIMEOUT_NS * 3));
    uart.advance(200 + TIMEOUT_NS * 3);
    assert!(uart.interrupt_asserted());
    assert_eq!(read(&mut uart, reg::DR), u32::from(b'b'));
    assert!(uart.interrupt_asserted());
    assert_eq!(read(&mut uart, reg::DR), u32::from(b'c'));
    assert!(!uart.interrupt_asserted());
    assert_eq!(uart.next_timeout(), None);
    assert_eq!(read(&mut uart, reg::FR), reg::FR_RXFE | reg::FR_TXFE);
}

#[test]
fn receive_timeout_handles_counter_wrap_and_cancelled_timers() {
    let mut uart = SbsaUart::new();
    let start = u64::MAX - 100;
    write(&mut uart, reg::IMSC, reg::INT_RT, start);
    assert!(uart.receive(1, start));
    assert_eq!(uart.next_timeout(), Some(TIMEOUT_NS - 101));
    uart.advance(TIMEOUT_NS - 102);
    assert_eq!(read(&mut uart, reg::RIS), 0);
    uart.advance(TIMEOUT_NS - 101);
    assert_eq!(read(&mut uart, reg::RIS), reg::INT_RT);
    assert_eq!(read(&mut uart, reg::DR), 1);
    assert!(uart.receive(2, TIMEOUT_NS));
    assert_eq!(read(&mut uart, reg::DR), 2);
    assert_eq!(uart.next_timeout(), None);
    uart.advance(TIMEOUT_NS * 2);
    assert_eq!(read(&mut uart, reg::RIS), 0);
}

#[test]
fn receive_timeout_uses_the_platform_counter_frequency() {
    for (frequency, timeout) in [
        (54_000_000, 15_000),
        (62_500_000, 17_362),
        (1_000_000_000, TIMEOUT_NS),
    ] {
        let mut uart = SbsaUart::with_clock(frequency);
        write(&mut uart, reg::IMSC, reg::INT_RT, 10);
        assert!(uart.receive(b'a', 10));
        assert_eq!(uart.next_timeout(), Some(10 + timeout));
        uart.advance(9 + timeout);
        assert!(!uart.interrupt_asserted());
        uart.advance(10 + timeout);
        assert!(uart.interrupt_asserted());
    }
}

#[test]
fn timeout_mask_gates_raw_status_and_reenabling_starts_a_fresh_interval() {
    let mut uart = SbsaUart::new();
    assert!(uart.receive(1, 0));
    assert_eq!(uart.next_timeout(), None);
    uart.advance(TIMEOUT_NS);
    assert_eq!(read(&mut uart, reg::RIS), 0);
    write(&mut uart, reg::IMSC, reg::INT_RT, TIMEOUT_NS);
    assert_eq!(uart.next_timeout(), Some(TIMEOUT_NS * 2));
    uart.advance(TIMEOUT_NS * 2);
    assert_eq!(read(&mut uart, reg::RIS), reg::INT_RT);
    assert_eq!(read(&mut uart, reg::MIS), reg::INT_RT);
    write(&mut uart, reg::IMSC, 0, TIMEOUT_NS * 2);
    assert_eq!(read(&mut uart, reg::RIS), 0);
    assert_eq!(uart.next_timeout(), None);
    write(&mut uart, reg::IMSC, reg::INT_RT, TIMEOUT_NS * 3);
    assert_eq!(uart.next_timeout(), Some(TIMEOUT_NS * 4));
    // An unrelated mask change must not defer an already running timer.
    write(
        &mut uart,
        reg::IMSC,
        reg::INT_RT | reg::INT_RX,
        TIMEOUT_NS * 3 + 1,
    );
    assert_eq!(uart.next_timeout(), Some(TIMEOUT_NS * 4));
    write(&mut uart, reg::IMSC, 0, TIMEOUT_NS * 3 + 2);
    uart.advance(TIMEOUT_NS * 4);
    assert_eq!(read(&mut uart, reg::RIS), 0);
}

#[test]
fn overrun_preserves_fifo_and_has_independent_error_and_irq_clears() {
    let mut uart = SbsaUart::new();
    write(&mut uart, reg::IMSC, reg::INT_OE, 0);
    for byte in 0..32 {
        assert!(uart.receive(byte, 0));
    }
    assert!(!uart.can_receive());
    assert!(!uart.receive(99, 0));
    assert!(uart.interrupt_asserted());
    assert_eq!(read(&mut uart, reg::RSR_ECR), reg::RSR_OE);
    assert_eq!(read(&mut uart, reg::FR), reg::FR_RXFF | reg::FR_TXFE);
    write(&mut uart, reg::ICR, reg::INT_OE, 0);
    assert!(!uart.interrupt_asserted());
    assert_eq!(read(&mut uart, reg::RIS), reg::INT_RX);
    assert_eq!(read(&mut uart, reg::RSR_ECR), reg::RSR_OE);
    assert_eq!(read(&mut uart, reg::DR), 0);
    assert!(uart.can_receive());
    assert!(uart.receive(32, 0));
    assert!(!uart.receive(100, 0));
    write(&mut uart, reg::RSR_ECR, 0, 0); // Any ECR value clears status.
    assert_eq!(read(&mut uart, reg::RSR_ECR), 0);
    assert!(uart.interrupt_asserted());
    for byte in 1..32 {
        assert_eq!(read(&mut uart, reg::DR), byte);
    }
    // The marker travels with the first byte accepted after overflow. ECR
    // clears status without rewriting already received FIFO entries.
    assert_eq!(read(&mut uart, reg::DR), 32 | reg::DR_OE);
    assert_eq!(read(&mut uart, reg::DR), 0);
    assert_eq!(read(&mut uart, reg::RIS), reg::INT_OE);
    write(&mut uart, reg::ICR, reg::INT_OE, 0);
    assert!(!uart.interrupt_asserted());
}

#[test]
fn ecr_clears_pending_overrun_before_the_next_byte_but_icr_preserves_it() {
    for (clear_register, expected_marker) in [(reg::RSR_ECR, 0), (reg::ICR, reg::DR_OE)] {
        let mut uart = SbsaUart::new();
        for byte in 0..32 {
            assert!(uart.receive(byte, 0));
        }
        assert!(!uart.receive(99, 0));
        write(&mut uart, clear_register, reg::INT_OE, 0);
        assert_eq!(read(&mut uart, reg::DR), 0);
        assert!(uart.receive(32, 0));
        for byte in 1..32 {
            assert_eq!(read(&mut uart, reg::DR), byte);
        }
        assert_eq!(read(&mut uart, reg::DR), 32 | expected_marker);
    }
}

#[test]
fn fifo_wrap_keeps_receive_and_transmit_order_independent() {
    let mut uart = SbsaUart::new();
    for round in 0..8 {
        for index in 0..32 {
            let byte = round * 32 + index;
            assert!(uart.receive(byte, 0));
            write(&mut uart, reg::DR, u32::from(255 - byte), 0);
        }
        for index in 0..32 {
            let byte = round * 32 + index;
            assert_eq!(read(&mut uart, reg::DR), u32::from(byte));
            assert_eq!(uart.take_transmitted(), Some(255 - byte));
        }
        assert!(uart.can_receive());
        assert!(uart.can_transmit());
    }
}

#[test]
fn accesses_use_register_bases_and_mask_width_without_exposing_pl011_features() {
    let mut uart = SbsaUart::new();
    for offset in [1, 2, 3, 0x1000, u64::MAX] {
        assert_eq!(uart.read(offset, 1), Err(AccessError::InvalidAccess));
        assert_eq!(uart.write(offset, 1, 1, 0), Err(AccessError::InvalidAccess));
    }
    for width in [0, 3, 8, usize::MAX] {
        assert_eq!(uart.read(reg::DR, width), Err(AccessError::InvalidAccess));
        assert_eq!(
            uart.write(reg::DR, width, 1, 0),
            Err(AccessError::InvalidAccess)
        );
    }
    for width in [1, 2, 4] {
        require_ok(uart.write(reg::DR, width, 0x1234_5658, 0));
        assert_eq!(read(&mut uart, reg::FR), reg::FR_RXFE | reg::FR_BUSY);
        assert_eq!(uart.take_transmitted(), Some(b'X'));
    }
    require_ok(uart.write(reg::IMSC, 1, u64::MAX, 0));
    assert_eq!(read(&mut uart, reg::IMSC), 0xf0);
    require_ok(uart.write(reg::IMSC, 2, u64::MAX, 0));
    assert_eq!(read(&mut uart, reg::IMSC), reg::INT_ALL);
    assert_eq!(require_ok(uart.read(reg::IMSC, 1)), 0xf0);
    for offset in [
        0x008, 0x020, 0x024, 0x028, 0x02c, 0x030, 0x034, 0x048, 0xfe0, 0xffc,
    ] {
        require_ok(uart.write(offset, 4, u64::MAX, 0));
        assert_eq!(require_ok(uart.read(offset, 4)), 0);
    }
    let flags = read(&mut uart, reg::FR);
    let raw = read(&mut uart, reg::RIS);
    for offset in [reg::FR, reg::RIS, reg::MIS] {
        write(&mut uart, offset, u32::MAX, 0);
    }
    assert_eq!(read(&mut uart, reg::FR), flags);
    assert_eq!(read(&mut uart, reg::RIS), raw);
    assert_eq!(read(&mut uart, reg::ICR), 0);
}

fn read(uart: &mut SbsaUart, offset: u64) -> u32 {
    require_ok(uart.read(offset, 4)) as u32
}

fn write(uart: &mut SbsaUart, offset: u64, value: u32, now_ns: u64) {
    require_ok(uart.write(offset, 4, u64::from(value), now_ns));
}
