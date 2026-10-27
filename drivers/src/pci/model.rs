// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded PCI configuration and MSI-X shadow state. No function registers.

use super::{APERTURE_SIZE, ECAM_SIZE, MAX_VECTORS, MSI_FRAME_OFFSET};

pub const BAR_AREA_OFFSET: u64 = 0x20_0000;
pub const MSI_SETSPI_OFFSET: u64 = 0x40;
pub const MSIX_CAPABILITY: usize = 0x40;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BarLayout {
    pub index: u32,
    pub address: u64,
    pub size: u64,
    /// Bit zero denotes a 64-bit BAR; bit one denotes prefetchability.
    pub flags: u32,
    pub offset: u64,
}

/// Pack the largest BAR first, then fill aligned gaps. The aperture is bounded
/// independently of firmware's often multi-gigabyte outbound window.
pub fn place_bars(bars: &mut [Option<BarLayout>; 6]) -> bool {
    let mut placed = [false; 6];
    for _ in 0..6 {
        let Some(index) = (0..6)
            .filter(|index| !placed[*index] && bars[*index].is_some())
            .max_by_key(|index| bars[*index].map(|bar| bar.size))
        else {
            return true;
        };
        let Some(mut bar) = bars[index] else {
            return false;
        };
        if bar.size < 16 || !bar.size.is_power_of_two() || bar.size > APERTURE_SIZE {
            return false;
        }
        let mut offset = BAR_AREA_OFFSET;
        loop {
            let Some(aligned) = offset
                .checked_add(bar.size - 1)
                .map(|value| value & !(bar.size - 1))
            else {
                return false;
            };
            offset = aligned;
            let Some(end) = offset
                .checked_add(bar.size)
                .filter(|end| *end <= APERTURE_SIZE)
            else {
                return false;
            };
            let overlap = (0..6)
                .filter(|other| placed[*other])
                .filter_map(|other| bars[other])
                .find(|other| offset < other.offset + other.size && other.offset < end);
            if let Some(other) = overlap {
                offset = other.offset + other.size;
            } else {
                break;
            }
        }
        bar.offset = offset;
        bars[index] = Some(bar);
        placed[index] = true;
    }
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MsixLayout {
    pub count: u32,
    pub table: u32,
    pub pending: u32,
}
impl MsixLayout {
    pub fn valid(self, bars: &[Option<BarLayout>; 6]) -> bool {
        if self.count == 0 || self.count as usize > MAX_VECTORS {
            return false;
        }
        let table_size = u64::from(self.count) * 16;
        let pending_size = u64::from(self.count.div_ceil(64)) * 8;
        let inside = |descriptor: u32, size: u64| {
            bars.get((descriptor & 7) as usize)
                .copied()
                .flatten()
                .is_some_and(|bar| {
                    u64::from(descriptor & !7)
                        .checked_add(size)
                        .is_some_and(|end| end <= bar.size)
                })
        };
        if !inside(self.table, table_size) || !inside(self.pending, pending_size) {
            return false;
        }
        self.table & 7 != self.pending & 7
            || !overlap(
                u64::from(self.table & !7),
                table_size,
                u64::from(self.pending & !7),
                pending_size,
            )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarAccess {
    Registers { index: usize, offset: usize },
    Msix { vector: usize, offset: usize },
    Pending { offset: usize },
}

/// One exclusive assignment owns this state under its device lock. A table
/// entry may name only its assigned guest SPI range and virtual MSI doorbell.
/// It never contains a guest-selected physical MSI address.
pub struct FunctionState {
    bars: [Option<BarLayout>; 6],
    probes: [bool; 6],
    msix: MsixLayout,
    vectors: [[u32; 4]; MAX_VECTORS],
    delivered: [Option<u32>; MAX_VECTORS],
    identity: u32,
    class_revision: u32,
    subsystem: u32,
    guest_base: u64,
    irq_base: u32,
    command: u16,
    enabled: bool,
    masked: bool,
    active: bool,
    pending_irq: Option<u32>,
    // An ECAM assignment exposes only validated, read-only transport
    // capabilities. Power management, reset, ATS and MSI controls stay hidden.
    transport_config: Option<[u32; 64]>,
}
impl FunctionState {
    pub fn new(
        bars: [Option<BarLayout>; 6],
        msix: MsixLayout,
        identity: u32,
        class_revision: u32,
        subsystem: u32,
        guest_base: u64,
        irq_base: u32,
    ) -> Self {
        let mut bars = bars;
        for bar in bars.iter_mut().flatten() {
            bar.address = bar.offset;
        }
        Self {
            bars,
            probes: [false; 6],
            msix,
            vectors: [[0, 0, 0, 1]; MAX_VECTORS],
            delivered: [None; MAX_VECTORS],
            identity,
            class_revision,
            subsystem,
            guest_base,
            irq_base,
            command: 0,
            enabled: false,
            masked: true,
            active: false,
            pending_irq: None,
            transport_config: None,
        }
    }
    pub(super) fn with_transport_config(mut self, config: [u32; 64]) -> Self {
        self.transport_config = Some(config);
        self
    }
    pub fn intx_enabled(&self) -> bool {
        self.active && self.command & 0x400 == 0
    }
    pub fn activate(&mut self) {
        self.active = true;
        self.commit_routes();
    }
    pub fn stop(&mut self) {
        self.active = false;
        self.pending_irq = None;
    }
    pub fn bus_master(&self) -> bool {
        self.active && self.command & 4 != 0
    }
    pub fn msix_enabled(&self) -> bool {
        self.active && self.enabled
    }
    pub fn msix_masked(&self) -> bool {
        !self.active || self.masked
    }
    pub fn msi_irq(&self, vector: usize) -> Option<u32> {
        if !self.active || !self.enabled || self.masked || vector >= self.msix.count as usize {
            return None;
        }
        let entry = self.vectors[vector];
        if entry[3] & 1 != 0 {
            return None;
        }
        let address = (u64::from(entry[1]) << 32) | u64::from(entry[0]);
        (address == self.guest_base + MSI_FRAME_OFFSET + MSI_SETSPI_OFFSET
            && self.valid_irq(entry[2]))
        .then_some(entry[2])
    }
    /// Masking cannot retract an MSI already queued at the host controller.
    /// Retain its last committed route until the assignment is stopped. As on
    /// hardware, the driver must quiesce/drain before repurposing a vector;
    /// the physical MSI does not carry an earlier table-routing generation.
    pub fn delivered_irq(&self, vector: usize) -> Option<u32> {
        if !self.active {
            return None;
        }
        self.delivered.get(vector).copied().flatten()
    }
    fn commit_routes(&mut self) {
        for vector in 0..self.msix.count as usize {
            if let Some(irq) = self.msi_irq(vector) {
                self.delivered[vector] = Some(irq);
            }
        }
    }
    fn valid_irq(&self, irq: u32) -> bool {
        irq.checked_sub(self.irq_base)
            .is_some_and(|offset| offset < self.msix.count)
    }
    pub fn take_pending_irq(&mut self) -> Option<u32> {
        self.pending_irq.take()
    }
    pub fn frame_read(&self, offset: usize, width: usize) -> Option<u64> {
        if width != 4 || !offset.is_multiple_of(4) || offset >= 4096 {
            return None;
        }
        Some(match offset {
            0x008 => u64::from((self.irq_base << 16) | self.msix.count),
            0xfcc => 0, // No implementation-specific MSI data quirks.
            _ => 0,
        })
    }
    pub fn frame_write(&mut self, offset: usize, width: usize, value: u64) -> bool {
        if width != 4 || !offset.is_multiple_of(4) || offset >= 4096 {
            return false;
        }
        if offset == MSI_SETSPI_OFFSET as usize && self.active && self.valid_irq(value as u32) {
            self.pending_irq = Some(value as u32);
        }
        true
    }
    pub fn config_read(&self, offset: usize, width: usize) -> Option<u64> {
        if !valid_access(offset, width, ECAM_SIZE as usize) || width > 4 {
            return None;
        }
        if offset >= 4096 {
            return Some(width_mask(width));
        }
        let word = self.config_word(offset & !3);
        Some((u64::from(word) >> ((offset & 3) * 8)) & width_mask(width))
    }
    fn config_word(&self, offset: usize) -> u32 {
        if let Some(config) = &self.transport_config
            && (offset == 0x34 || offset >= 0x3c)
        {
            return config.get(offset / 4).copied().unwrap_or(0);
        }
        match offset {
            0 => self.identity,
            4 => u32::from(self.command) | (1 << 20),
            8 => self.class_revision,
            0x0c => 0,
            0x10..=0x24 => self.bar_word((offset - 0x10) / 4),
            0x2c => self.subsystem,
            0x34 => MSIX_CAPABILITY as u32,
            MSIX_CAPABILITY if self.msix.count != 0 => {
                0x11 | ((self.msix.count - 1) << 16)
                    | (u32::from(self.masked) << 30)
                    | (u32::from(self.enabled) << 31)
            }
            0x44 => self.msix.table,
            0x48 => self.msix.pending,
            _ => 0,
        }
    }
    fn bar_word(&self, index: usize) -> u32 {
        if let Some(bar) = self.bars[index] {
            let value = if self.probes[index] {
                !(bar.size - 1)
            } else {
                bar.address
            };
            return (value as u32 & !15)
                | if bar.flags & 1 != 0 { 4 } else { 0 }
                | if bar.flags & 2 != 0 { 8 } else { 0 };
        }
        if index > 0
            && let Some(bar) = self.bars[index - 1]
            && bar.flags & 1 != 0
        {
            return ((if self.probes[index] {
                !(bar.size - 1)
            } else {
                bar.address
            }) >> 32) as u32;
        }
        0
    }
    pub fn config_write(&mut self, offset: usize, width: usize, value: u64) -> bool {
        if !valid_access(offset, width, ECAM_SIZE as usize) || width > 4 {
            return false;
        }
        if offset >= 4096 {
            return true;
        }
        let register = offset & !3;
        let mask = (width_mask(width) as u32) << ((offset & 3) * 8);
        let word =
            (self.config_word(register) & !mask) | (((value as u32) << ((offset & 3) * 8)) & mask);
        match register {
            4 => self.command = word as u16 & (2 | 4 | 0x400),
            MSIX_CAPABILITY if self.transport_config.is_none() => {
                self.enabled = word & (1 << 31) != 0;
                self.masked = word & (1 << 30) != 0;
            }
            0x10..=0x24 => return self.write_bar((register - 0x10) / 4, word),
            _ => {} // Read-only header/capabilities; no power/reset/vendor writes.
        }
        self.commit_routes();
        true
    }
    fn write_bar(&mut self, index: usize, word: u32) -> bool {
        if word == u32::MAX {
            self.probes[index] = true;
            return true;
        }
        self.probes[index] = false;
        let (owner, high) = if self.bars[index].is_some() {
            (index, false)
        } else if index > 0 && self.bars[index - 1].is_some_and(|bar| bar.flags & 1 != 0) {
            (index - 1, true)
        } else {
            return true;
        };
        let Some(mut bar) = self.bars[owner] else {
            return false;
        };
        let address = if high {
            (u64::from(word) << 32) | (bar.address & 0xffff_ffff)
        } else {
            (bar.address & !0xffff_ffff) | u64::from(word & !15)
        };
        // Address zero disables a BAR while the guest resource allocator probes.
        if address != 0
            && (!address.is_multiple_of(bar.size)
                || address < BAR_AREA_OFFSET
                || address
                    .checked_add(bar.size)
                    .is_none_or(|end| end > APERTURE_SIZE)
                || self.bars.iter().enumerate().any(|(other, existing)| {
                    other != owner
                        && existing.is_some_and(|other| {
                            other.address != 0
                                && overlap(address, bar.size, other.address, other.size)
                        })
                }))
        {
            return false;
        }
        bar.address = address;
        self.bars[owner] = Some(bar);
        true
    }
    pub fn bar_access(&self, offset: usize, width: usize) -> Option<BarAccess> {
        if !valid_access(offset, width, APERTURE_SIZE as usize) || self.command & 2 == 0 {
            return None;
        }
        // PCI bus addresses are offsets in the guest host bridge ranges.
        let address = offset as u64;
        let (index, bar) = self.bars.iter().enumerate().find_map(|(index, bar)| {
            bar.filter(|bar| {
                bar.address != 0
                    && address >= bar.address
                    && address
                        .checked_add(width as u64)
                        .is_some_and(|end| end <= bar.address + bar.size)
            })
            .map(|bar| (index, bar))
        })?;
        let local = address - bar.address;
        if self.msix.table & 7 == index as u32
            && overlap(
                local,
                width as u64,
                u64::from(self.msix.table & !7),
                u64::from(self.msix.count) * 16,
            )
        {
            let table = local.checked_sub(u64::from(self.msix.table & !7))? as usize;
            if table + width > self.msix.count as usize * 16 || table % 16 + width > 16 {
                return None;
            }
            return Some(BarAccess::Msix {
                vector: table / 16,
                offset: table % 16,
            });
        }
        if self.msix.pending & 7 == index as u32
            && overlap(
                local,
                width as u64,
                u64::from(self.msix.pending & !7),
                u64::from(self.msix.count.div_ceil(64)) * 8,
            )
        {
            let pending = local.checked_sub(u64::from(self.msix.pending & !7))? as usize;
            if pending + width > self.msix.count.div_ceil(64) as usize * 8 {
                return None;
            }
            return Some(BarAccess::Pending { offset: pending });
        }
        Some(BarAccess::Registers {
            index,
            offset: local as usize,
        })
    }
    pub fn table_read(&self, vector: usize, offset: usize, width: usize) -> Option<u64> {
        if vector >= self.msix.count as usize || !valid_access(offset, width, 16) {
            return None;
        }
        let entry = self.vectors[vector];
        let mut result = 0;
        for byte in 0..width {
            let index = offset + byte;
            result |= u64::from((entry[index / 4] >> ((index % 4) * 8)) & 255) << (byte * 8);
        }
        Some(result)
    }
    pub fn table_write(&mut self, vector: usize, offset: usize, width: usize, value: u64) -> bool {
        if vector >= self.msix.count as usize || !valid_access(offset, width, 16) {
            return false;
        }
        for byte in 0..width {
            let index = offset + byte;
            let shift = (index % 4) * 8;
            let word = &mut self.vectors[vector][index / 4];
            *word = (*word & !(255 << shift)) | ((((value >> (byte * 8)) & 255) as u32) << shift);
        }
        self.vectors[vector][3] &= 1;
        if let Some(irq) = self.msi_irq(vector) {
            self.delivered[vector] = Some(irq);
        }
        true
    }
}
fn valid_access(offset: usize, width: usize, size: usize) -> bool {
    matches!(width, 1 | 2 | 4 | 8)
        && offset.is_multiple_of(width)
        && offset.checked_add(width).is_some_and(|end| end <= size)
}
fn width_mask(width: usize) -> u64 {
    u64::MAX >> (64 - width * 8)
}
fn overlap(first: u64, length: u64, second: u64, other_length: u64) -> bool {
    first < second + other_length && second < first + length
}
