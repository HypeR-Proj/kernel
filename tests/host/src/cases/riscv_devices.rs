// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::vm::device::uart16550::Ns16550;
use hyper::vm::riscv64::plic::VirtualPlic;

#[test]
fn uart_fifo_trigger_and_sparse_character_timeout() {
    let mut uart = Ns16550::with_clock(3_686_400, 3_686_400);
    uart.write_at(3, 3, 0); // Eight data bits, one stop bit.
    uart.write_at(2, 0x41, 0); // Four-byte trigger.
    uart.write_at(1, 1, 0);
    assert!(uart.receive(b'a', 0));
    assert!(!uart.interrupt_asserted());
    assert_eq!(uart.next_timeout(), Some(640));
    uart.advance(639);
    assert!(!uart.interrupt_asserted());
    uart.advance(640);
    assert_eq!(uart.read_at(2, 640), 0xcc);
    assert_eq!(uart.next_timeout(), None);
    assert_eq!(uart.read_at(0, 640), b'a');
    assert!(!uart.interrupt_asserted());
    for byte in b"abcd" {
        assert!(uart.receive(*byte, 700));
    }
    assert_eq!(uart.read_at(2, 700), 0xc4);
    assert_eq!(uart.read_at(0, 700), b'a');
    assert!(!uart.interrupt_asserted());
    assert_eq!(uart.next_timeout(), Some(1340));
}

#[test]
fn uart_reset_overrun_dlab_and_thre_latch() {
    let mut uart = Ns16550::new();
    uart.write(1, 2);
    assert_eq!(uart.read(2), 2);
    assert_eq!(uart.read(2), 1);
    assert_eq!(uart.write(0, b'x'), Some(b'x'));
    assert_eq!(uart.read(2), 2);
    uart.write(3, 0x80);
    uart.write(0, 12);
    uart.write(1, 3);
    assert_eq!(uart.read(0), 12);
    assert_eq!(uart.read(1), 3);
    uart.write(3, 3);
    uart.write(1, 5);
    assert!(uart.receive(1, 0));
    assert!(!uart.receive(2, 0));
    assert_eq!(uart.read(2), 6);
    assert_eq!(uart.read(5) & 3, 3);
    assert_eq!(uart.read(2), 4);
    uart.write(2, 1);
    assert_eq!(uart.read(5) & 3, 0);
    for value in 0..16 {
        assert!(uart.receive(value, 0));
    }
    assert!(!uart.can_receive());
    uart.write(2, 3);
    assert!(uart.can_receive());
    assert_eq!(uart.next_timeout(), None);
}

#[test]
fn uart_loopback_does_not_transmit_to_host() {
    let mut uart = Ns16550::new();
    uart.write(4, 0x1f);
    assert_eq!(uart.read(6) & 0xf0, 0xf0);
    assert_eq!(uart.write(0, 0x5a), None);
    assert_eq!(uart.read(0), 0x5a);
}

#[test]
fn plic_claim_threshold_and_level_replay() -> Result<(), hyper::vm::riscv64::plic::Error> {
    let mut plic = VirtualPlic::new();
    plic.write(10 * 4, 3)?;
    plic.write(11 * 4, 3)?;
    plic.write(0x2080, (1 << 10) | (1 << 11))?;
    plic.write(0x201000, 3)?;
    plic.set_level(11, true)?;
    plic.set_level(10, true)?;
    assert!(!plic.interrupt_asserted());
    assert_eq!(plic.read(0x201004)?, 10); // Tie: smallest source, despite threshold.
    assert_eq!(plic.read(0x201004)?, 11);
    assert_eq!(plic.read(0x201004)?, 0);
    plic.write(0x201000, 0)?;
    plic.write(0x201004, 10)?;
    assert!(plic.interrupt_asserted());
    assert_eq!(plic.read(0x201004)?, 10);
    plic.set_level(10, false)?;
    plic.write(0x201004, 10)?;
    assert!(!plic.interrupt_asserted());
    plic.set_level(11, false)?;
    plic.write(0x201004, 11)?;
    assert_eq!(plic.read(0x1000)?, 0);
    assert!(plic.set_level(32, true).is_err());
    assert!(plic.read(1).is_err());
    Ok(())
}

#[test]
fn uart_timeout_restart_and_counter_wrap() {
    let mut uart = Ns16550::with_clock(3_686_400, 3_686_400);
    uart.write_at(3, 3, 0);
    uart.write_at(2, 0xc1, 0);
    uart.write_at(1, 1, 0);
    let start = u64::MAX - 200;
    assert!(uart.receive(1, start));
    assert_eq!(uart.next_timeout(), Some(439));
    uart.advance(438);
    assert!(!uart.interrupt_asserted());
    // A stale timer callback may race with a newly received character.
    assert!(uart.receive(2, 400));
    uart.advance(439);
    assert!(!uart.interrupt_asserted());
    assert_eq!(uart.next_timeout(), Some(1040));
    uart.advance(1040);
    assert!(uart.interrupt_asserted());
    uart.write_at(2, 0, 1040);
    assert_eq!(uart.next_timeout(), None);
    assert_eq!(uart.read_at(5, 1040) & 1, 0);
}

#[test]
fn plic_masking_reserved_sources_and_in_service_gateway()
-> Result<(), hyper::vm::riscv64::plic::Error> {
    let mut plic = VirtualPlic::new();
    plic.write(0, u32::MAX)?;
    assert_eq!(plic.read(0)?, 0);
    plic.write(4, 1)?;
    plic.set_level(1, true)?;
    plic.set_level(1, false)?;
    assert_eq!(plic.read(0x1000)?, 2); // Deassertion does not retract a latched request.
    assert_eq!(plic.read(0x201004)?, 0); // Disabled source cannot be claimed.
    plic.write(0x2080, 3)?;
    assert_eq!(plic.read(0x2080)?, 2);
    assert_eq!(plic.read(0x201004)?, 1);
    plic.set_level(1, true)?;
    assert_eq!(plic.read(0x1000)?, 0); // Gateway remains busy until completion.
    plic.write(0x201004, 2)?; // Wrong source must not release source1.
    assert_eq!(plic.read(0x1000)?, 0);
    plic.write(0x201004, 1)?;
    assert_eq!(plic.read(0x1000)?, 2);
    assert_eq!(plic.read(0x200000)?, 0); // No machine delivery context.
    Ok(())
}

#[test]
fn uart_loopback_excludes_external_input_without_losing_internal_data() {
    let mut uart = Ns16550::new();
    uart.write(4, 16);
    assert!(!uart.can_receive_external());
    assert!(!uart.receive_external(b'X', 0));
    assert_eq!(uart.write(0, b'L'), None);
    assert!(!uart.receive_external(b'Y', 0));
    assert_eq!(uart.read(5) & 3, 1); // No external overrun or data insertion.
    assert_eq!(uart.read(0), b'L');
    uart.write(4, 0);
    assert!(uart.can_receive_external());
    assert!(uart.receive_external(b'Z', 0));
    assert_eq!(uart.read(0), b'Z');
}
