// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! NS16550 register-width, stride, MMIO, and port-I/O contracts.

use hyper::drivers::platform::{MmioResource, PermanentMmioMapping};
use hyper::drivers::serial::{
    MmioAccess, Ns16550, Ns16550DataBits, Ns16550Error, Ns16550FifoTrigger, Ns16550LineConfig,
    Ns16550Parity, Ns16550StopBits,
};
use hyper::hal::console::Console;
use hyper::hal::io::PortIo;
use std::sync::Mutex;

static PORTS: Mutex<[u8; 8]> = Mutex::new([0; 8]);

fn test_mapping(base: usize, size: u64) -> PermanentMmioMapping {
    let range = crate::require_some(hyper::platform::PhysicalRange::new(base as u64, size));
    // SAFETY: This test assigns the live register array to the test UART.
    let resource = unsafe { MmioResource::from_physical_range(range) };
    // SAFETY: The caller supplies a live test register array covering `size`;
    // the returned capability is used only within that array's scope.
    crate::require_ok(unsafe {
        PermanentMmioMapping::new(resource, hyper::mm::VirtualAddress::new(base as u64))
    })
}

unsafe fn read_port(port: u16) -> u8 {
    let Ok(ports) = PORTS.lock() else {
        return 0;
    };
    ports
        .get(usize::from(port.saturating_sub(0x3f8)))
        .copied()
        .unwrap_or(0)
}

unsafe fn write_port(port: u16, value: u8) {
    let Ok(mut ports) = PORTS.lock() else {
        return;
    };
    if let Some(register) = ports.get_mut(usize::from(port.saturating_sub(0x3f8))) {
        *register = value;
    }
}

#[test]
fn configures_and_uses_a_byte_wide_register_bank() {
    let mut registers = [0u8; 8];
    registers[5] = (1 << 5) | (1 << 6);
    let mapping = test_mapping(registers.as_mut_ptr() as usize, registers.len() as u64);
    let uart = crate::require_ok(Ns16550::from_mapped_mmio(mapping, MmioAccess::BYTE));
    crate::require_ok(uart.configure(
        24_000_000,
        115_200,
        Ns16550LineConfig::EIGHT_N_ONE,
        Ns16550FifoTrigger::FourteenBytes,
    ));

    assert_eq!(registers[0], 13);
    assert_eq!(registers[3], 3);
    assert_eq!(registers[2], 0xc7);
    assert_eq!(registers[4], 0x0b);
    registers[5] = 1 | (1 << 1) | (1 << 5);
    registers[0] = b'R';
    let received = crate::require_some(uart.try_read());
    assert_eq!(received.byte, b'R');
    assert!(received.overrun_error);
    assert!(received.has_error());
    registers[5] = 1 << 5;
    assert!(uart.try_write(b'T'));
    assert_eq!(registers[0], b'T');
}

#[test]
fn honors_word_access_and_register_shift() {
    let mut registers = [0u32; 8];
    registers[5] = 1 << 5;
    let mapping = test_mapping(
        registers.as_mut_ptr() as usize,
        core::mem::size_of_val(&registers) as u64,
    );
    let uart = crate::require_ok(Ns16550::from_mapped_mmio(mapping, MmioAccess::WORD));
    uart.write_byte(b'W');
    assert_eq!(registers[0], u32::from(b'W'));
    uart.write_scratch(0x5a);
    assert_eq!(uart.read_scratch(), 0x5a);

    let line = Ns16550LineConfig {
        data_bits: Ns16550DataBits::Seven,
        stop_bits: Ns16550StopBits::Two,
        parity: Ns16550Parity::Even,
    };
    crate::require_ok(uart.configure(1_843_200, 9_600, line, Ns16550FifoTrigger::OneByte));
    assert_eq!(registers[0], 12);
    assert_eq!(registers[3], 0x1e);
}

#[test]
fn rejects_a_register_stride_whose_window_overflows() {
    let mut registers = [0u8; 8];
    let mapping = test_mapping(registers.as_mut_ptr() as usize, registers.len() as u64);
    assert_eq!(
        Ns16550::from_mapped_mmio(mapping, MmioAccess::Byte { register_shift: 63 },).map(|_| ()),
        Err(Ns16550Error::AddressOverflow)
    );
}

#[test]
fn accepts_the_exact_sparse_register_window() {
    let registers = [0u32; 15];
    let mapping = test_mapping(
        registers.as_ptr() as usize,
        core::mem::size_of_val(&registers) as u64,
    );
    assert!(Ns16550::from_mapped_mmio(mapping, MmioAccess::Word { register_shift: 3 }).is_ok());
}

#[test]
fn uses_an_explicit_port_io_capability() {
    if let Ok(mut ports) = PORTS.lock() {
        *ports = [0; 8];
        ports[5] = 1 << 5;
    }
    // SAFETY: The callbacks model exactly one byte access in the owned
    // eight-port test bank.
    let io = unsafe { PortIo::new(read_port, write_port) };
    // SAFETY: This test owns all eight simulated ports beginning at 0x3f8 for
    // the lifetime of the UART handle.
    let uart = crate::require_ok(unsafe { Ns16550::from_port(0x3f8, io) });
    crate::require_ok(uart.configure(
        1_843_200,
        115_200,
        Ns16550LineConfig::EIGHT_N_ONE,
        Ns16550FifoTrigger::OneByte,
    ));
    uart.write_byte(b'P');
    let ports = match PORTS.lock() {
        Ok(ports) => ports,
        Err(_) => panic!("test port bank lock was poisoned"),
    };
    assert_eq!(ports[0], b'P');
    assert_eq!(ports[3], 3);
    assert_eq!(ports[4], 0x0b);
}
