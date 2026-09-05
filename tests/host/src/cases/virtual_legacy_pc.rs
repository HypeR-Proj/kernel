// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Legacy x86 virtual-device composition and interrupt routing.

use hyper::vm::x86::device::legacy_pc::{InterruptSource, LegacyPcDevices};

#[test]
fn keeps_uart_divisor_programming_out_of_the_console_stream() {
    let mut devices = LegacyPcDevices::new();
    let _ = crate::require_ok(devices.access(0x3fb, 1, true, 0x80));
    let divisor_low = crate::require_ok(devices.access(0x3f8, 1, true, 1));
    let divisor_high = crate::require_ok(devices.access(0x3f9, 1, true, 0));
    assert_eq!(divisor_low.transmitted, None);
    assert_eq!(divisor_high.transmitted, None);

    let _ = crate::require_ok(devices.access(0x3fb, 1, true, 0x03));
    let output = crate::require_ok(devices.access(0x3f8, 1, true, u32::from(b'X')));
    assert_eq!(output.transmitted, Some(b'X'));
    let status = crate::require_ok(devices.access(0x3fd, 1, false, 0));
    assert_eq!(status.value, Some(0x60));
}

#[test]
fn routes_the_pit_only_after_the_master_pic_unmasks_irq_zero() {
    let mut devices = LegacyPcDevices::new();
    assert_eq!(devices.timer_vector(), None);

    for (port, value) in [(0x20, 0x11), (0x21, 0x20), (0x21, 0x04), (0x21, 0x01)] {
        let _ = crate::require_ok(devices.access(port, 1, true, value));
    }
    let _ = crate::require_ok(devices.access(0x21, 1, true, 0xfe));
    assert_eq!(devices.timer_vector(), Some(0x20));
}

#[test]
fn routes_uart_transmit_interrupts_through_pic_irq_four() {
    let mut devices = LegacyPcDevices::new();
    for (port, value) in [(0x20, 0x11), (0x21, 0x20), (0x21, 0x04), (0x21, 0x01)] {
        let _ = crate::require_ok(devices.access(port, 1, true, value));
    }
    let _ = crate::require_ok(devices.access(0x21, 1, true, 0xef));
    let _ = crate::require_ok(devices.access(0x3f9, 1, true, 1 << 1));
    let output = crate::require_ok(devices.access(0x3f8, 1, true, u32::from(b'H')));
    assert_eq!(output.transmitted, Some(b'H'));

    let interrupt = crate::require_some(devices.pending_interrupt(false));
    assert_eq!(interrupt.vector, 0x24);
    assert_eq!(interrupt.source, InterruptSource::Com1);
    let identification = crate::require_ok(devices.access(0x3fa, 1, false, 0));
    assert_eq!(identification.value, Some(0x02));
    assert_eq!(devices.pending_interrupt(false), None);
}

#[test]
fn returns_an_absent_device_value_for_unimplemented_ports() {
    let mut devices = LegacyPcDevices::new();
    let access = crate::require_ok(devices.access(0x0cf8, 4, false, 0));
    assert_eq!(access.value, Some(u32::MAX));
}
