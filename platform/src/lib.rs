// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Firmware discovery and platform resource descriptions.

#![no_std]

pub(crate) mod bcm2712;

pub mod chosen;
pub mod fdt;
mod pci;

pub use hyper::mm::boot::MAX_MEMORY_REGIONS;
pub use hyper::mm::boot::MAX_RESERVED_REGIONS;
pub const MAX_MMIO_REGIONS: usize = 64;
pub const MAX_NO_MAP_REGIONS: usize = 32;
/// Maximum number of independent contiguous GICR ranges retained during
/// allocation-free discovery. One range may contain Redistributors for many
/// CPUs; this is deliberately independent from [`MAX_CPUS`].
/// Like the memory and MMIO range lists, this is early-discovery scratch
/// capacity, not an independently configurable hardware or CPU count.
pub const MAX_GIC_REDISTRIBUTOR_REGIONS: usize = 16;
pub const MAX_PLIC_CONTEXTS: usize = MAX_CPUS;
pub const MAX_CPUS: usize = hyper::config::MAX_CPUS as usize;

pub use hyper::mm::ranges::{CapacityError, PhysicalRange, RegionList};

/// Hardware facts discovered from firmware-provided platform data.
#[derive(Clone, Copy, Debug)]
pub struct PlatformInfo {
    pub cpus: CpuList,
    pub memory: RegionList<MAX_MEMORY_REGIONS>,
    pub reserved: RegionList<MAX_RESERVED_REGIONS>,
    pub no_map: RegionList<MAX_NO_MAP_REGIONS>,
    pub mmio: RegionList<MAX_MMIO_REGIONS>,
    pub dtb_size: u64,
}

/// Firmware-discovered processing element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuInfo {
    pub hardware_id: u64,
}

/// Fixed-capacity CPU topology available before the heap is initialized.
#[derive(Clone, Copy, Debug)]
pub struct CpuList {
    entries: [CpuInfo; MAX_CPUS],
    length: usize,
}

impl CpuList {
    pub const fn new() -> Self {
        Self {
            entries: [CpuInfo { hardware_id: 0 }; MAX_CPUS],
            length: 0,
        }
    }

    pub fn as_slice(&self) -> &[CpuInfo] {
        &self.entries[..self.length]
    }

    pub const fn len(&self) -> usize {
        self.length
    }

    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn push(&mut self, cpu: CpuInfo) -> Result<(), CpuListError> {
        if self.entries[..self.length]
            .iter()
            .any(|entry| entry.hardware_id == cpu.hardware_id)
        {
            return Err(CpuListError::Duplicate);
        }
        if self.length == MAX_CPUS {
            return Err(CpuListError::Capacity);
        }
        self.entries[self.length] = cpu;
        self.length += 1;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuListError {
    Capacity,
    Duplicate,
}

impl Default for CpuList {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuPowerInfo {
    Psci(PsciInfo),
    Sbi(SbiInfo),
    X86Apic(X86ApicInfo),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SbiInfo;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X86ApicInfo {
    pub tsc_frequency: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PsciInfo {
    pub conduit: PsciConduit,
    pub interface: PsciInterface,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PsciConduit {
    Smc,
    Hvc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PsciCompatibleVersion {
    V0_2,
    V1_0,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PsciInterface {
    Standard(PsciCompatibleVersion),
    Legacy(PsciLegacyFunctionIds),
}

/// Firmware-assigned PSCI 0.1 function IDs from the legacy DT binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PsciLegacyFunctionIds {
    pub cpu_suspend: Option<u32>,
    pub cpu_off: u32,
    pub cpu_on: u32,
    pub migrate: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimerInfo {
    pub kind: TimerKind,
    pub virtual_timer: PlatformInterrupt,
    pub hypervisor_physical: PlatformInterrupt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerKind {
    ArmGeneric,
    RiscvSupervisor,
    X86TscDeadline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlatformInterruptTrigger {
    Level,
    Edge,
}

// Essential discovery runs before the heap exists, so the bounded GIC region
// list must remain inline rather than boxing the architecture-specific variant.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy, Debug)]
pub enum InterruptControllerInfo {
    GicV2(GicV2Info),
    GicV3(GicV3Info),
    Plic(PlicInfo),
    X2Apic(X2ApicInfo),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X2ApicInfo;

#[derive(Clone, Copy, Debug)]
pub struct PlicInfo {
    pub registers: PhysicalRange,
    pub source_count: u32,
    /// Supervisor context index indexed by the firmware hardware-thread ID.
    pub supervisor_contexts: [u32; MAX_PLIC_CONTEXTS],
    pub context_count: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct GicV2Info {
    pub distributor: PhysicalRange,
    pub cpu_interface: PhysicalRange,
    pub hypervisor_interface: Option<PhysicalRange>,
    pub virtual_cpu_interface: Option<PhysicalRange>,
    pub maintenance_interrupt: Option<PlatformInterrupt>,
}

#[derive(Clone, Copy, Debug)]
pub struct GicV3Info {
    pub distributor: PhysicalRange,
    pub redistributors: RegionList<MAX_GIC_REDISTRIBUTOR_REGIONS>,
    pub redistributor_stride: Option<u64>,
    pub maintenance_interrupt: Option<PlatformInterrupt>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlatformInterrupt {
    pub interrupt: u32,
    pub trigger: PlatformInterruptTrigger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConsoleInfo {
    pub kind: ConsoleKind,
    pub base: u64,
    pub access: ConsoleRegisterAccess,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleRegisterAccess {
    Native,
    Port,
    Mmio8 { register_shift: u8 },
    Mmio32 { register_shift: u8 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsoleKind {
    Pl011,
    Ns16550,
}
