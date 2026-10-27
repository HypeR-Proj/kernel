// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

pub mod console;

mod ns16550;
mod pl011;

pub use ns16550::{
    DataBits as Ns16550DataBits, Error as Ns16550Error, FifoTrigger as Ns16550FifoTrigger,
    InterruptIdentification as Ns16550InterruptIdentification,
    InterruptMask as Ns16550InterruptMask, LineConfig as Ns16550LineConfig, MmioAccess,
    ModemStatus as Ns16550ModemStatus, Ns16550, PLATFORM_DRIVER as NS16550_PLATFORM_DRIVER,
    Parity as Ns16550Parity, ReceivedByte as Ns16550ReceivedByte, StopBits as Ns16550StopBits,
};
pub use pl011::{
    DataBits, Error, FifoLevel, InterruptStatus, LineConfig,
    PLATFORM_DRIVER as PL011_PLATFORM_DRIVER, Parity, Pl011, ReceivedByte, StopBits,
};
