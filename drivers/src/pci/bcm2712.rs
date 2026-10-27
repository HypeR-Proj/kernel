// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! BCM2712 `PCIe` host bridge: link handoff, BAR access and DMA translation.
//!
//! One exclusive bridge owner prepares routing before function assignment.
//!
//! DT bounds CPU mapping authority; it does not describe current PCI register
//! contents. A valid outbound window outside that reservation can be relocated
//! without changing its PCI target or endpoint BARs.
//!
//! Inbound DMA uses validated firmware ranges with endpoint bus mastering
//! disabled. Firmware must already have drained DMA at Image entry; disabling
//! bus mastering here is not a drain or isolation proof.

use super::model::BarLayout;
use super::{Error, Registers};
use crate::platform::MmioResource;
use hyper::mm::ranges::PhysicalRange;

pub const DMA_OFFSET: u64 = 0x10_0000_0000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkFailure {
    NotRootPort,
    LinkDown,
    BusNumbersRejected,
}

/// A failed firmware handoff can be diagnosed without touching endpoint
/// configuration space, whose reads may abort while the physical link is down.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct LinkState {
    pub failure: LinkFailure,
    pub bridge: u64,
    pub status: u32,
    pub bus_numbers: u32,
    pub control: u32,
}

impl core::fmt::Debug for LinkState {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "LinkState {{ failure: {:?}, bridge: {:#x}, status: {:#010x}, bus_numbers: {:#010x}, control: {:#010x} }}",
            self.failure, self.bridge, self.status, self.bus_numbers, self.control
        )
    }
}

#[derive(Clone, Copy)]
pub(super) struct HostBridge {
    registers: Registers,
    endpoint: u32,
}

impl HostBridge {
    pub(super) fn prepare(registers: Registers) -> Result<Self, Error> {
        // These registers belong to the host bridge and are safe to sample
        // before establishing whether endpoint configuration reads are allowed.
        let status = registers.read(0x4068);
        let bus_numbers = registers.read(0x18);
        let control = registers.read(0x4064);
        let link_error = |failure, bus_numbers| {
            Error::Link(LinkState {
                failure,
                bridge: registers.0.resource().start(),
                status,
                bus_numbers,
                control,
            })
        };
        let failure = if status & 0x80 == 0 {
            Some(LinkFailure::NotRootPort)
        } else if status & 0x30 != 0x30 {
            Some(LinkFailure::LinkDown)
        } else {
            None
        };
        if let Some(failure) = failure {
            return Err(link_error(failure, bus_numbers));
        }

        // Firmware can leave a live link and programmed address windows but
        // no PCI bus numbers. This driver exclusively owns one root port and
        // one direct endpoint: enumerate bus 0 -> bus 1, with no further buses.
        // Retain valid firmware numbering; preserve the secondary latency byte
        // when replacing an unconfigured or inconsistent routing tuple.
        let secondary = (bus_numbers >> 8) & 0xff;
        let subordinate = (bus_numbers >> 16) & 0xff;
        let routed_buses = if bus_numbers & 0xff == 0 && secondary != 0 && subordinate >= secondary
        {
            bus_numbers
        } else {
            (bus_numbers & 0xff00_0000) | 0x0001_0100
        };
        if routed_buses != bus_numbers {
            registers.write(0x18, routed_buses);
        }
        // Complete and verify routing before issuing any endpoint config access.
        let observed = registers.read(0x18);
        if observed != routed_buses {
            return Err(link_error(LinkFailure::BusNumbersRejected, observed));
        }
        let this = Self {
            registers,
            endpoint: ((routed_buses >> 8) & 0xff) << 20,
        };
        if matches!(this.read(0) & 0xffff, 0 | 0xffff) || this.read(0x0c) & 0x00ff_0000 != 0 {
            return Err(Error::Identity);
        }
        // The AArch64 Image boot protocol requires all DMA producers quiesced.
        // Keep bus mastering gated throughout validation and IRQ preparation.
        let command = this.read(4) & 0xffff;
        this.write(4, (command & !5) | 0x400);
        this.write(0x30, this.read(0x30) & !1);
        if command & 2 == 0 || this.read(4) & 4 != 0 {
            return Err(Error::Handoff);
        }
        Ok(this)
    }

    pub(super) fn read(self, offset: usize) -> u32 {
        self.registers.write(0x9000, self.endpoint);
        self.registers.read(0x8000 + offset)
    }

    pub(super) fn write(self, offset: usize, value: u32) {
        self.registers.write(0x9000, self.endpoint);
        self.registers.write(0x8000 + offset, value);
    }

    pub(super) fn bus_master(self, enabled: bool) {
        let command = self.read(4) & 0xffff;
        self.write(4, (command & !4) | if enabled { 4 } else { 0 });
        let _ = self.read(4);
    }

    /// Size BARs only while both decoding and bus mastering are disabled.
    /// Firmware addresses are restored before any failure is returned.
    pub(super) fn bars(self) -> Result<[Option<BarLayout>; 6], Error> {
        let command = self.read(4) & 0xffff & !4;
        self.write(4, command & !3);
        let result = (|| {
            let mut bars = [None; 6];
            let mut index = 0;
            while index < bars.len() {
                let offset = 0x10 + index * 4;
                let low = self.read(offset);
                if low & 1 != 0 || low & 6 == 2 || low & 6 == 6 {
                    return Err(Error::BarType {
                        index: index as u32,
                        value: low,
                    });
                }
                let wide = low & 6 == 4;
                if wide && index == 5 {
                    return Err(Error::BarType {
                        index: index as u32,
                        value: low,
                    });
                }
                let high = if wide { self.read(offset + 4) } else { 0 };
                self.write(offset, u32::MAX);
                if wide {
                    self.write(offset + 4, u32::MAX);
                }
                let mask_low = self.read(offset);
                let mask_high = if wide { self.read(offset + 4) } else { 0 };
                if wide {
                    self.write(offset + 4, high);
                }
                self.write(offset, low);
                let address = (u64::from(high) << 32) | u64::from(low & !15);
                let mask = (u64::from(mask_high) << 32) | u64::from(mask_low & !15);
                let invalid_size = Error::BarSize {
                    index: index as u32,
                    address,
                    mask,
                };
                let size = if wide {
                    (!mask).checked_add(1)
                } else {
                    Some(u64::from((!(mask_low & !15)).wrapping_add(1)))
                }
                .ok_or(invalid_size)?;
                if size != 0 {
                    if size < 16 || !size.is_power_of_two() || !address.is_multiple_of(size) {
                        return Err(invalid_size);
                    }
                    bars[index] = Some(BarLayout {
                        index: index as u32,
                        address,
                        size,
                        flags: u32::from(wide) | if low & 8 != 0 { 2 } else { 0 },
                        offset: 0,
                    });
                }
                index += if wide { 2 } else { 1 };
            }
            Ok(bars)
        })();
        self.write(4, command);
        result
    }

    pub(super) fn msix(self) -> Result<(usize, usize, u32, u32), Error> {
        if self.read(4) & (1 << 20) == 0 {
            return Err(Error::Interrupt);
        }
        let mut offset = self.read(0x34) as usize & 0xff;
        let mut visited = 0u64;
        let mut found = None;
        while offset != 0 {
            if !(0x40..=0xfc).contains(&offset) || !offset.is_multiple_of(4) {
                return Err(Error::Interrupt);
            }
            let bit = 1u64 << (offset / 4);
            if visited & bit != 0 {
                return Err(Error::Interrupt);
            }
            visited |= bit;
            let header = self.read(offset);
            match header & 0xff {
                0x05 => {
                    // Firmware may have enabled ordinary MSI even on a
                    // function which also supports MSI-X. It is not exposed.
                    self.write(offset, header & !(1 << 16));
                }
                0x11 => {
                    if offset > 0xf4 || found.is_some() {
                        return Err(Error::Interrupt);
                    }
                    let count = ((header >> 16) & 0x7ff) as usize + 1;
                    self.write(offset, (header & !0x8000_0000) | 0x4000_0000);
                    if !(1..=64).contains(&count) {
                        return Err(Error::Interrupt);
                    }
                    found = Some((offset, count, self.read(offset + 4), self.read(offset + 8)));
                }
                _ => {}
            }
            offset = (header >> 8) as usize & 0xff;
        }
        found.ok_or(Error::Interrupt)
    }

    pub(super) fn configure_dma(self, plan: &DmaPlan) -> Result<(), Error> {
        if self.read(4) & 4 != 0 {
            return Err(Error::Handoff);
        }
        plan.install(self.registers)
    }

    pub(super) fn bar_resource(
        self,
        bar: BarLayout,
        aperture: MmioResource,
    ) -> Result<MmioResource, Error> {
        OutboundWindows::read(self.registers).resource(bar, aperture)
    }

    pub(super) fn prepare_outbound(
        self,
        bars: &[Option<BarLayout>; 6],
        aperture: MmioResource,
    ) -> Result<(), Error> {
        if self.read(4) & 4 != 0 {
            return Err(Error::Handoff);
        }
        OutboundWindows::read(self.registers).prepare(self.registers, bars, aperture)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboundFailure {
    NoWindow,
    AddressOverflow,
    OutsideAperture,
    AmbiguousTranslation,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct OutboundWriteState {
    pub register: usize,
    pub expected: u32,
    pub observed: u32,
    pub mask: u32,
}

impl core::fmt::Debug for OutboundWriteState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "OutboundWriteState {{ register: {:#x}, expected: {:#010x}, observed: {:#010x}, mask: {:#010x} }}",
            self.register, self.expected, self.observed, self.mask
        )
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct OutboundWindow {
    bus_low: u32,
    bus_high: u32,
    base_limit: u32,
    base_high: u32,
    limit_high: u32,
}

impl OutboundWindow {
    fn cpu(self) -> Option<PhysicalRange> {
        let start = ((u64::from(self.base_high & 0xff) << 12)
            | u64::from((self.base_limit >> 4) & 0xfff))
            << 20;
        let end = (((u64::from(self.limit_high & 0xff) << 12) | u64::from(self.base_limit >> 20))
            + 1)
            << 20;
        PhysicalRange::new(start, end.checked_sub(start)?)
    }

    fn bus(self) -> u64 {
        (u64::from(self.bus_high) << 32) | u64::from(self.bus_low)
    }

    fn pci_range(self) -> Option<PhysicalRange> {
        PhysicalRange::new(self.bus(), self.cpu()?.size())
    }
}

impl core::fmt::Debug for OutboundWindow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "[{:08x}:{:08x}/{:08x}/{:08x}/{:08x}]",
            self.bus_high, self.bus_low, self.base_limit, self.base_high, self.limit_high
        )
    }
}

/// A failed translation includes every window, so a hardware report does not
/// need another image just to distinguish missing, shifted or overlapping maps.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct OutboundState {
    pub failure: OutboundFailure,
    pub bar: u32,
    pub address: u64,
    pub size: u64,
    pub cpu_aperture: u64,
    windows: [OutboundWindow; 4],
}

impl core::fmt::Debug for OutboundState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "OutboundState {{ failure: {:?}, bar: {}, address: {:#x}, size: {:#x}, cpu_aperture: {:#x}, windows(bus_hi:lo/base_limit/base_hi/limit_hi): {:?} }}",
            self.failure, self.bar, self.address, self.size, self.cpu_aperture, self.windows
        )
    }
}

/// Snapshot of the host bridge's CPU-to-PCI address translations.
struct OutboundWindows([OutboundWindow; 4]);

impl OutboundWindows {
    fn read(registers: Registers) -> Self {
        Self(core::array::from_fn(|index| OutboundWindow {
            bus_low: registers.read(0x400c + index * 8),
            bus_high: registers.read(0x4010 + index * 8),
            base_limit: registers.read(0x4070 + index * 4),
            base_high: registers.read(0x4080 + index * 8),
            limit_high: registers.read(0x4084 + index * 8),
        }))
    }

    /// Retain usable mappings. Relocation is limited to one unambiguous live
    /// window containing every BAR; it must be disjoint from the reservation.
    /// The exclusive bridge owner has already gated endpoint bus mastering.
    fn prepare(
        &self,
        registers: Registers,
        bars: &[Option<BarLayout>; 6],
        aperture: MmioResource,
    ) -> Result<(), Error> {
        let mut missing = None;
        for bar in bars.iter().flatten().copied() {
            match self.resource(bar, aperture) {
                Ok(_) => {}
                Err(
                    error @ Error::Outbound(OutboundState {
                        failure: OutboundFailure::NoWindow,
                        ..
                    }),
                ) => {
                    missing.get_or_insert(error);
                }
                Err(error) => return Err(error),
            }
        }
        let Some(error) = missing else { return Ok(()) };
        let (index, size) = self.relocation(bars, aperture).ok_or(error)?;
        relocate_outbound_window(registers, index, aperture.start(), size)?;
        let configured = Self::read(registers);
        for bar in bars.iter().flatten().copied() {
            configured.resource(bar, aperture)?;
        }
        Ok(())
    }

    fn relocation(
        &self,
        bars: &[Option<BarLayout>; 6],
        aperture: MmioResource,
    ) -> Option<(usize, u64)> {
        const MIB: u64 = 1 << 20;
        if !aperture.start().is_multiple_of(MIB) || aperture.end() > 1 << 40 {
            return None;
        }
        let mut candidate = None;
        for (index, window) in self.0.iter().copied().enumerate() {
            let Some(cpu) = window.cpu() else { continue };
            let Some(pci) = window.pci_range() else {
                continue;
            };
            if cpu.overlaps(aperture.physical_range()) || !pci.start().is_multiple_of(MIB) {
                continue;
            }
            let mut size = 0;
            for bar in bars.iter().flatten() {
                let end = bar.address.checked_add(bar.size)?;
                if bar.address < pci.start() || end > pci.end() {
                    size = 0;
                    break;
                }
                size = size.max(end - pci.start());
            }
            let size = size.checked_add(MIB - 1)? & !(MIB - 1);
            if size == 0 || size > aperture.size() {
                continue;
            }
            let destination = PhysicalRange::new(aperture.start(), size)?;
            if candidate.is_some() {
                return None;
            }
            for (other, window) in self.0.iter().copied().enumerate() {
                if other == index {
                    continue;
                }
                let Some(other_cpu) = window.cpu() else {
                    continue;
                };
                let other_pci = window.pci_range()?;
                if other_cpu.overlaps(cpu) || other_cpu.overlaps(destination) {
                    return None;
                }
                for bar in bars.iter().flatten() {
                    if other_pci.overlaps(PhysicalRange::new(bar.address, bar.size)?) {
                        return None;
                    }
                }
            }
            candidate = Some((index, size));
        }
        candidate
    }

    /// Admit one complete BAR in the reserved CPU aperture. Different BARs may
    /// use different windows, but neither CPU decoding nor BAR translation may
    /// be ambiguous. The exclusive bridge owner keeps this snapshot stable.
    fn resource(&self, bar: BarLayout, aperture: MmioResource) -> Result<MmioResource, Error> {
        let error = |failure| {
            Error::Outbound(OutboundState {
                failure,
                bar: bar.index,
                address: bar.address,
                size: bar.size,
                cpu_aperture: aperture.start(),
                windows: self.0,
            })
        };
        let end = bar
            .address
            .checked_add(bar.size)
            .ok_or_else(|| error(OutboundFailure::AddressOverflow))?;
        let mut matched = None;
        for (index, window) in self.0.iter().enumerate() {
            let Some(cpu) = window.cpu() else { continue };
            if !cpu.overlaps(aperture.physical_range()) {
                continue;
            }
            let bus_end = window
                .bus()
                .checked_add(cpu.size())
                .ok_or_else(|| error(OutboundFailure::AddressOverflow))?;
            if bar.address < window.bus() || end > bus_end {
                continue;
            }
            let start = cpu
                .start()
                .checked_add(bar.address - window.bus())
                .ok_or_else(|| error(OutboundFailure::AddressOverflow))?;
            let resource = PhysicalRange::new(start, bar.size)
                .ok_or_else(|| error(OutboundFailure::AddressOverflow))?;
            if start < aperture.start() || resource.end() > aperture.end() {
                return Err(error(OutboundFailure::OutsideAperture));
            }
            // Even a window targeting unrelated PCI addresses conflicts if
            // it also decodes any byte of this CPU mapping.
            if matched.is_some()
                || self.0.iter().enumerate().any(|(other, window)| {
                    other != index && window.cpu().is_some_and(|cpu| cpu.overlaps(resource))
                })
            {
                return Err(error(OutboundFailure::AmbiguousTranslation));
            }
            // SAFETY: the live bridge translates this entire BAR into a
            // checked subrange of the firmware-owned CPU MMIO capability.
            matched = Some(unsafe { MmioResource::from_physical_range(resource) });
        }
        matched.ok_or_else(|| error(OutboundFailure::NoWindow))
    }
}

fn relocate_outbound_window(
    registers: Registers,
    index: usize,
    cpu: u64,
    size: u64,
) -> Result<(), Error> {
    let base_limit = 0x4070 + index * 4;
    let base_high = 0x4080 + index * 8;
    let limit_high = base_high + 4;
    let last = cpu + size - 1;
    let limit = ((last >> 20) as u32 & 0xfff) << 20;
    let base = ((cpu >> 20) as u32 & 0xfff) << 4;
    // Raising the old base and lowering its limit can only shrink its decode.
    // Once disabled, intermediate ranges are empty or a subset of the final
    // reservation: hold the low base at its maximum until the last write.
    // The PCI target registers are deliberately unchanged.
    for (offset, value, mask) in [
        (base_high, 0xff, 0xff),
        (base_limit, 0xfff0, 0xfff0_fff0),
        (limit_high, (last >> 32) as u32, 0xff),
        (base_limit, limit | 0xfff0, 0xfff0_fff0),
        (base_high, (cpu >> 32) as u32, 0xff),
        (base_limit, limit | base, 0xfff0_fff0),
    ] {
        let expected = (registers.read(offset) & !mask) | (value & mask);
        registers.write(offset, expected);
        let observed = registers.read(offset);
        if observed & mask != expected & mask {
            return Err(Error::OutboundWrite(OutboundWriteState {
                register: offset,
                expected,
                observed,
                mask,
            }));
        }
    }
    Ok(())
}

const DMA_WINDOW_COUNT: usize = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaStage {
    DisableWindow,
    ProgramWindow,
    EnableAccess,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct DmaState {
    pub stage: DmaStage,
    pub bridge: u64,
    pub register: usize,
    pub expected: u32,
    pub observed: u32,
    pub mask: u32,
}

impl core::fmt::Debug for DmaState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "DmaState {{ stage: {:?}, bridge: {:#x}, register: {:#x}, expected: {:#010x}, observed: {:#010x}, mask: {:#010x} }}",
            self.stage, self.bridge, self.register, self.expected, self.observed, self.mask
        )
    }
}

#[derive(Clone, Copy)]
struct DmaWindow {
    bus: PhysicalRange,
    cpu: PhysicalRange,
    encoding: u32,
}

/// Validated inbound DMA windows, installed only with bus mastering disabled.
pub(super) struct DmaPlan([Option<DmaWindow>; DMA_WINDOW_COUNT]);

impl DmaPlan {
    /// Support the fixed physical DMA offset advertised by this transport,
    /// its MSI page, and optional loopback ranges inside its owned MMIO aperture.
    /// Neither arbitrary `SoC` MMIO nor an additional RAM alias is admitted.
    pub(super) fn from_firmware(
        bytes: &[u8],
        aperture: MmioResource,
        message: u64,
        mip_cpu: u64,
    ) -> Option<Self> {
        const ENTRY_SIZE: usize = 7 * 4;
        if bytes.is_empty()
            || !bytes.len().is_multiple_of(ENTRY_SIZE)
            || bytes.len() / ENTRY_SIZE > DMA_WINDOW_COUNT
        {
            return None;
        }
        let mut windows: [Option<DmaWindow>; DMA_WINDOW_COUNT] = [None; DMA_WINDOW_COUNT];
        let mut ram = false;
        let mut msi = false;
        for (index, entry) in bytes.chunks_exact(ENTRY_SIZE).enumerate() {
            let cell = |index: usize| -> Option<u32> {
                Some(u32::from_be_bytes(
                    entry[index * 4..index * 4 + 4].try_into().ok()?,
                ))
            };
            let flags: u32 = cell(0)?;
            if flags & !0x4300_0000 != 0
                || !matches!(flags & 0x0300_0000, 0x0200_0000 | 0x0300_0000)
            {
                return None;
            }
            let pair = |index| Some((u64::from(cell(index)?) << 32) | u64::from(cell(index + 1)?));
            let size = pair(5)?;
            let bus = PhysicalRange::new(pair(1)?, size)?;
            let cpu = PhysicalRange::new(pair(3)?, size)?;
            let encoding = inbound_size_encoding(size)?;
            if !bus.start().is_multiple_of(size)
                || !cpu.start().is_multiple_of(4096)
                || bus.end() > 1u64 << 40
                || cpu.end() > 1u64 << 40
                || (flags & 0x0300_0000 == 0x0200_0000 && bus.end() > 1u64 << 32)
                || windows
                    .iter()
                    .flatten()
                    .any(|other| other.bus.overlaps(bus))
            {
                return None;
            }
            if bus.start() == DMA_OFFSET && cpu.start() == 0 && size == DMA_OFFSET {
                if ram {
                    return None;
                }
                ram = true;
            } else if bus.start() == message && cpu.start() == mip_cpu && size == 4096 {
                if msi {
                    return None;
                }
                msi = true;
            } else if cpu.start() < aperture.start() || cpu.end() > aperture.end() {
                return None;
            }
            windows[index] = Some(DmaWindow { bus, cpu, encoding });
        }
        (ram && msi).then_some(Self(windows))
    }

    /// All fallible topology and BAR validation precedes this call. Failure
    /// leaves the unpublished function's bus mastering gated; never restore
    /// an unvalidated firmware DMA layout or expose a partially installed one.
    fn install(&self, registers: Registers) -> Result<(), Error> {
        // Close every decoder before changing any address. Unused firmware
        // windows must not survive as hidden aliases into RAM or other MMIO.
        for index in 0..DMA_WINDOW_COUNT {
            let (bar, remap) = inbound_offsets(index);
            write_dma_checked(
                registers,
                DmaStage::DisableWindow,
                bar,
                registers.read(bar) & !31,
                31,
            )?;
            write_dma_checked(
                registers,
                DmaStage::DisableWindow,
                remap,
                registers.read(remap) & !1,
                1,
            )?;
        }
        for (index, window) in self
            .0
            .iter()
            .enumerate()
            .filter_map(|(i, w)| w.map(|w| (i, w)))
        {
            let (bar, remap) = inbound_offsets(index);
            // Install both addresses while the window is disabled. Publish
            // its size last; bus mastering stays closed until VM activation.
            for (offset, value, mask) in [
                (bar + 4, (window.bus.start() >> 32) as u32, 0xff),
                (bar, window.bus.start() as u32, u32::MAX),
                (remap + 4, (window.cpu.start() >> 32) as u32, 0xff),
                (remap, window.cpu.start() as u32 | 1, 0xffff_f001),
                (bar, window.bus.start() as u32 | window.encoding, u32::MAX),
            ] {
                write_dma_checked(registers, DmaStage::ProgramWindow, offset, value, mask)?;
            }
        }
        // SCB access gates the bridge's upstream path. Preserve firmware's
        // link, burst, QoS and other transport settings.
        write_dma_checked(
            registers,
            DmaStage::EnableAccess,
            0x4008,
            registers.read(0x4008) | 0x1000,
            0x1000,
        )
    }
}

fn inbound_offsets(index: usize) -> (usize, usize) {
    if index < 3 {
        (0x402c + index * 8, 0x40ac + index * 8)
    } else {
        (0x40d4 + (index - 3) * 8, 0x410c + (index - 3) * 8)
    }
}

fn write_dma_checked(
    registers: Registers,
    stage: DmaStage,
    offset: usize,
    value: u32,
    mask: u32,
) -> Result<(), Error> {
    registers.write(offset, value);
    let observed = registers.read(offset);
    if observed & mask == value & mask {
        Ok(())
    } else {
        Err(Error::Dma(DmaState {
            stage,
            bridge: registers.0.resource().start(),
            register: offset,
            expected: value,
            observed,
            mask,
        }))
    }
}

fn inbound_size_encoding(size: u64) -> Option<u32> {
    if !size.is_power_of_two() {
        return None;
    }
    match size.trailing_zeros() {
        bits @ 12..=15 => Some(bits + 16),
        bits @ 16..=36 => Some(bits - 15),
        _ => None,
    }
}

/// BCM2712's inbound size field has a discontinuity for 4--32 KiB windows.
pub fn inbound_size(encoded: u32) -> Option<u64> {
    match encoded {
        1..=21 => Some(1u64 << (encoded + 15)),
        28..=31 => Some(1u64 << (encoded - 16)),
        _ => None,
    }
}
