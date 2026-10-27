// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral events reported by guest execution.
//!
//! This module describes guest-visible facts consumed by VM policy. It does
//! not own raw exception frames, decode architecture syndromes, or apply
//! actions to hardware state; those responsibilities remain in the selected
//! architecture backend.

/// The memory operation which caused a guest-memory fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryAccess {
    Execute,
    Read,
    Write,
}

/// A power-of-two guest access width supported by the initial device models.
///
/// Keeping width validation at decode boundaries prevents an unchecked byte
/// count from reaching device policy or architecture-local completion logic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessWidth {
    Byte,
    HalfWord,
    Word,
    DoubleWord,
}

impl AccessWidth {
    pub const fn from_bytes(bytes: usize) -> Option<Self> {
        Some(match bytes {
            1 => Self::Byte,
            2 => Self::HalfWord,
            4 => Self::Word,
            8 => Self::DoubleWord,
            _ => return None,
        })
    }

    pub const fn bytes(self) -> usize {
        match self {
            Self::Byte => 1,
            Self::HalfWord => 2,
            Self::Word => 4,
            Self::DoubleWord => 8,
        }
    }
}

/// One decoded access to a memory-mapped virtual device.
///
/// The architecture backend retains the raw exception frame and any register
/// completion metadata. VM policy receives only this owned description.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioAccess {
    address: GuestPhysicalAddress,
    width: AccessWidth,
    operation: MmioOperation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmioOperation {
    Read,
    Write(u64),
}

impl MmioAccess {
    pub const fn new(
        address: GuestPhysicalAddress,
        width: AccessWidth,
        operation: MmioOperation,
    ) -> Self {
        Self {
            address,
            width,
            operation,
        }
    }

    pub const fn address(self) -> GuestPhysicalAddress {
        self.address
    }

    pub const fn width(self) -> AccessWidth {
        self.width
    }

    pub const fn size(self) -> usize {
        self.width.bytes()
    }

    pub const fn operation(self) -> MmioOperation {
        self.operation
    }
}

/// Kernel policy selected for a decoded guest-memory fault.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum MemoryFaultAction {
    /// The mapping was installed; retry the faulting instruction unchanged.
    Retry,
    /// RAM policy did not own the address; continue device-access decoding.
    ForwardToDevice,
    /// The exit cannot safely return to the guest.
    Stop,
}

/// Kernel policy selected for one MMIO operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum MmioAction {
    /// Complete a read with the supplied guest-visible value.
    CompleteRead(u64),
    /// Complete a write after its side effects committed.
    CompleteWrite,
    /// Registered userspace device owns this access; detach before publishing
    /// its request and resume only after exact-generation completion.
    Deferred,
    /// No installed virtual device owns the address.
    Unhandled,
    /// Device policy failed after the exit was decoded.
    Stop,
}

/// An address in a virtual machine's physical address space.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct GuestPhysicalAddress(u64);

impl GuestPhysicalAddress {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A failed access resolved through the guest's second-stage address space.
///
/// Architecture backends preserve the most specific fact common to `AArch64`
/// stage-2 faults, `RISC-V` guest-page faults, and x86 EPT/NPT violations. In
/// particular, this event does not claim that the failure was caused by a
/// missing translation: some architectures report permission failures through
/// the same exit class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestMemoryFault {
    address: GuestPhysicalAddress,
    access: MemoryAccess,
    during_guest_page_walk: bool,
}

impl GuestMemoryFault {
    pub const fn new(
        address: GuestPhysicalAddress,
        access: MemoryAccess,
        during_guest_page_walk: bool,
    ) -> Self {
        Self {
            address,
            access,
            during_guest_page_walk,
        }
    }

    pub const fn address(self) -> GuestPhysicalAddress {
        self.address
    }

    pub const fn access(self) -> MemoryAccess {
        self.access
    }

    /// Returns whether hardware faulted while walking a guest page table.
    pub const fn during_guest_page_walk(self) -> bool {
        self.during_guest_page_walk
    }
}
