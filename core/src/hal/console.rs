// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::fmt;

/// Minimal byte-oriented output contract used during early kernel startup.
///
/// Rich terminal policy belongs above this interface. Drivers only provide a
/// reliable byte sink, which keeps this contract usable across architectures.
pub trait Console: Sync {
    /// Attempts one byte without waiting for transmitter capacity.
    fn try_write_byte(&self, byte: u8) -> bool;

    /// Writes one byte, waiting until the transmitter accepts it.
    ///
    /// Boot paths may use this blocking operation. Runtime and emergency
    /// output use [`Self::try_write_byte`] with their respective scheduling or
    /// fixed-budget policy so a stalled UART cannot monopolize a CPU.
    fn write_byte(&self, byte: u8) {
        while !self.try_write_byte(byte) {
            core::hint::spin_loop();
        }
    }

    fn write_bytes(&self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_byte(byte);
        }
    }
}

pub struct ConsoleWriter<'a>(pub &'a dyn Console);

impl fmt::Write for ConsoleWriter<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.write_bytes(text.as_bytes());
        Ok(())
    }
}
