// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

/// Architecture-provided access to an isolated I/O-port address space.
#[derive(Clone, Copy)]
pub struct PortIo {
    read: unsafe fn(u16) -> u8,
    write: unsafe fn(u16, u8),
}

impl PortIo {
    /// Creates a capability backed by architecture-owned port instructions.
    ///
    /// # Safety
    ///
    /// The callbacks must issue exactly one ordered byte access to `port` and
    /// must remain callable for the kernel lifetime.
    pub const unsafe fn new(read: unsafe fn(u16) -> u8, write: unsafe fn(u16, u8)) -> Self {
        Self { read, write }
    }

    /// Reads one byte from a port owned by the caller.
    ///
    /// # Safety
    ///
    /// The caller must own `port`, and the platform must permit the access.
    pub unsafe fn read(self, port: u16) -> u8 {
        // SAFETY: PortIo::new requires this callback to implement exactly one
        // ordered byte access, and this method forwards its caller contract.
        unsafe { (self.read)(port) }
    }

    /// Writes one byte to a port owned by the caller.
    ///
    /// # Safety
    ///
    /// The caller must own `port`, and the platform must permit the access.
    pub unsafe fn write(self, port: u16, value: u8) {
        // SAFETY: PortIo::new validates the callback contract; ownership and
        // platform permission are inherited from this unsafe method's caller.
        unsafe { (self.write)(port, value) };
    }
}
