// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Modern virtio PCI functions behind a firmware-described generic ECAM host.
//!
//! Only transport capabilities are exposed. `INTx` is routed by the host; MSI,
//! MSI-X, ATS, power and reset controls remain inaccessible to the guest. In
//! particular, no interrupt-controller page needs to enter its DMA domain.

use super::{Bar, Error, FunctionState, Prepared, Registers, Transport, model, subrange};
use alloc::vec::Vec;
use {crate::platform::DriverServices, crate::platform::PlatformDevice};
use {
    hyper::hal::barrier::Barrier, hyper::hal::barrier::BarrierAccess,
    hyper::hal::barrier::BarrierDomain,
};

pub(super) struct Function {
    config: Registers,
    pub bars: [Option<Bar>; 6],
    layouts: [Option<model::BarLayout>; 6],
    capabilities: [u32; 64],
    pub stream: u32,
}

fn word(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    bytes
        .get(offset..offset + 4)
        .and_then(|v| v.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or(Error::Firmware)
}
fn property(node: &PlatformDevice, name: &str) -> Result<u32, Error> {
    let value = node.property(name).ok_or(Error::Firmware)?;
    if value.len() != 4 {
        return Err(Error::Firmware);
    }
    word(value, 0)
}

pub(super) fn discover<B: Barrier>(
    nodes: &[PlatformDevice],
    services: &dyn DriverServices,
) -> Result<Vec<Prepared>, Error> {
    let mut hosts = nodes
        .iter()
        .filter(|node| node.is_compatible("pci-host-ecam-generic"));
    let mut functions = Vec::new();
    let Some(host) = hosts.next() else {
        return Ok(functions);
    };
    if hosts.next().is_some()
        || host.property("dma-coherent").is_none()
        || host.registers().len() != 1
        || word(host.property("bus-range").ok_or(Error::Firmware)?, 0)? != 0
    {
        return Err(Error::Firmware);
    }
    let mut smmus = nodes
        .iter()
        .filter(|node| node.is_compatible("arm,smmu-v3"));
    let smmu = smmus.next().ok_or(Error::Firmware)?;
    if smmus.next().is_some() {
        return Err(Error::Firmware);
    }
    let phandle = property(smmu, "phandle")?;
    let iommu_map = host.property("iommu-map").ok_or(Error::Firmware)?;
    let mask = match host.property("iommu-map-mask") {
        Some(bytes) if bytes.len() == 4 => word(bytes, 0)?,
        None => u32::MAX,
        _ => return Err(Error::Firmware),
    };
    let order = || B::data_memory(BarrierDomain::FullSystem, BarrierAccess::All);
    let config = services
        .map_mmio(host.registers()[0])
        .map_err(|_| Error::Mapping)?;
    config
        .validate_window(1 << 20, 4)
        .map_err(|_| Error::Mapping)?;
    let (bus, outbound) = host.pci_memory().ok_or(Error::Firmware)?;
    // Physical BARs share the host's bounded window. Their packing is
    // independent of each VM-facing ECAM/MSI/BAR aperture's virtual offsets.
    let mut cursor = bus;
    functions.try_reserve_exact(4).map_err(|_| Error::Mapping)?;
    for slot in 0..32 {
        let registers = services
            .map_mmio(subrange(config.resource(), slot << 15, 4096)?)
            .map_err(|_| Error::Mapping)?;
        let config = Registers(registers, order);
        let identity = config.read(0);
        if identity & 0xffff != 0x1af4 || !matches!(identity >> 16, 0x1041 | 0x1048) {
            continue;
        }
        // Admission is deliberately single-function: no sibling can share a
        // reset/interrupt boundary without being represented in its owner.
        if config.read(0x0c) & 0x00ff_0000 != 0 {
            return Err(Error::Identity);
        }
        config.write_width(4, 2, 0x400); // decoding and DMA off; INTx masked
        let capabilities = transport_capabilities(config)?;
        let mut layouts = probe_bars(config)?;
        if !model::place_bars(&mut layouts) {
            return Err(Error::BarPlacement);
        }
        let mut bars = [None; 6];
        for (index, layout) in layouts.iter_mut().enumerate() {
            let Some(layout) = layout else {
                continue;
            };
            let address = cursor
                .checked_add(layout.size - 1)
                .ok_or(Error::BarPlacement)?
                & !(layout.size - 1);
            if address
                .checked_add(layout.size)
                .is_none_or(|end| end > 1 << 32)
            {
                return Err(Error::BarPlacement);
            }
            let mapping = services
                .map_mmio(subrange(
                    outbound,
                    address.checked_sub(bus).ok_or(Error::BarPlacement)?,
                    layout.size,
                )?)
                .map_err(|_| Error::Mapping)?;
            config.write(0x10 + index * 4, address as u32);
            if layout.flags & 1 != 0 {
                config.write(0x14 + index * 4, (address >> 32) as u32);
            }
            layout.address = address;
            cursor = address + layout.size;
            bars[index] = Some(Bar {
                mapping,
                offset: layout.offset,
                flags: layout.flags,
                index: index as u32,
            });
        }
        validate_capabilities(&capabilities, &layouts)?;
        let requester = (slot << 3) as u16;
        let stream =
            crate::iommu::smmuv3::firmware::pci_stream_id(iommu_map, mask, requester, phandle)
                .map_err(|_| Error::Firmware)?;
        let interrupt = intx(nodes, host, requester, (config.read(0x3c) >> 8) & 255)?;
        if functions.iter().any(|old: &Prepared| {
            old.interrupt == interrupt || old.transport.stream() == Some(stream)
        }) {
            return Err(Error::Interrupt);
        }
        if functions.len() == 4 {
            return Err(Error::Handoff);
        }
        functions.push(Prepared {
            firmware: host.id(),
            bars,
            interrupt,
            interrupt_count: 1,
            transport: Transport::ecam(Function {
                config,
                bars,
                layouts,
                capabilities,
                stream,
            }),
        });
    }
    Ok(functions)
}

fn probe_bars(config: Registers) -> Result<[Option<model::BarLayout>; 6], Error> {
    let mut bars = [None; 6];
    let mut index = 0;
    while index < 6 {
        let offset = 0x10 + index * 4;
        let original = config.read(offset);
        if original & 1 != 0 || !matches!(original & 6, 0 | 4) {
            return Err(Error::BarType {
                index: index as u32,
                value: original,
            });
        }
        let wide = original & 6 == 4;
        if wide && index == 5 {
            return Err(Error::BarPlacement);
        }
        let high = if wide { config.read(offset + 4) } else { 0 };
        config.write(offset, u32::MAX);
        if wide {
            config.write(offset + 4, u32::MAX);
        }
        let low_mask = config.read(offset) & !15;
        let high_mask = if wide {
            config.read(offset + 4)
        } else {
            u32::MAX
        };
        config.write(offset, original);
        if wide {
            config.write(offset + 4, high);
        }
        if low_mask != 0 || (wide && high_mask != 0) {
            let mask = (u64::from(high_mask) << 32) | u64::from(low_mask);
            let size = (!mask).checked_add(1).ok_or(Error::BarPlacement)?;
            bars[index] = Some(model::BarLayout {
                index: index as u32,
                address: 0,
                size,
                flags: u32::from(wide) | ((original & 8) >> 2),
                offset: 0,
            });
        }
        index += if wide { 2 } else { 1 };
    }
    Ok(bars)
}

fn transport_capabilities(config: Registers) -> Result<[u32; 64], Error> {
    let mut shadow = [0; 64];
    shadow[0x3c / 4] = 1 << 8;
    let mut seen = [false; 64];
    let mut occupied = [false; 64];
    let mut next = (config.read(0x34) & 255) as usize;
    let mut previous = 0x34;
    while next != 0 {
        if !(0x40..=0xfc).contains(&next) || !next.is_multiple_of(4) || seen[next / 4] {
            return Err(Error::Firmware);
        }
        seen[next / 4] = true;
        let header = config.read(next);
        match header & 255 {
            0x05 => config.write_width(next + 2, 2, u64::from((header >> 16) & !1)),
            0x11 => config.write_width(next + 2, 2, u64::from((header >> 16) & !0x8000 | 0x4000)),
            0x09 if matches!(header >> 24, 1..=4) => {
                let length = ((header >> 16) & 255) as usize;
                if length < 16
                    || !length.is_multiple_of(4)
                    || next + length > 256
                    || (header >> 24 == 2 && length < 20)
                {
                    return Err(Error::Firmware);
                }
                if occupied[next / 4..(next + length) / 4]
                    .iter()
                    .any(|used| *used)
                {
                    return Err(Error::Firmware);
                }
                occupied[next / 4..(next + length) / 4].fill(true);
                for offset in (0..length).step_by(4) {
                    shadow[(next + offset) / 4] = config.read(next + offset);
                }
                shadow[next / 4] &= !0xff00;
                if previous == 0x34 {
                    shadow[previous / 4] = next as u32;
                } else {
                    shadow[previous / 4] |= (next as u32) << 8;
                }
                previous = next;
            }
            _ => {}
        }
        next = ((header >> 8) & 255) as usize;
    }
    Ok(shadow)
}

fn validate_capabilities(
    config: &[u32; 64],
    bars: &[Option<model::BarLayout>; 6],
) -> Result<(), Error> {
    let mut kinds = 0;
    let mut next = config[0x34 / 4] as usize;
    while next != 0 {
        let header = config[next / 4];
        let kind = header >> 24;
        let index = (config[next / 4 + 1] & 255) as usize;
        let bar = bars
            .get(index)
            .and_then(|bar| *bar)
            .ok_or(Error::BarPlacement)?;
        let offset = u64::from(config[next / 4 + 2]);
        let length = u64::from(config[next / 4 + 3]);
        let minimum = match kind {
            1 => 56,
            2 => 2,
            _ => 1,
        };
        if length < minimum || offset.checked_add(length).is_none_or(|end| end > bar.size) {
            return Err(Error::BarPlacement);
        }
        kinds |= 1 << kind;
        next = ((header >> 8) & 255) as usize;
    }
    if kinds != 0x1e {
        return Err(Error::Firmware);
    }
    Ok(())
}

fn intx(
    nodes: &[PlatformDevice],
    host: &PlatformDevice,
    requester: u16,
    pin: u32,
) -> Result<u32, Error> {
    if !(1..=4).contains(&pin) {
        return Err(Error::Interrupt);
    }
    let mask = host.property("interrupt-map-mask").ok_or(Error::Firmware)?;
    if mask.len() != 16 {
        return Err(Error::Firmware);
    }
    let key = [u32::from(requester) << 8, 0, 0, pin];
    let map = host.property("interrupt-map").ok_or(Error::Firmware)?;
    let mut offset = 0;
    let mut result = None;
    while offset < map.len() {
        let parent = word(map, offset + 16)?;
        let parent = nodes
            .iter()
            .find(|node| property(node, "phandle") == Ok(parent))
            .ok_or(Error::Firmware)?;
        if !(parent.is_compatible("arm,gic-v3")
            || parent.is_compatible("arm,gic-400")
            || parent.is_compatible("arm,cortex-a15-gic"))
            || property(parent, "#interrupt-cells")? != 3
        {
            return Err(Error::Firmware);
        }
        let addresses = match parent.property("#address-cells") {
            None => 0,
            Some(value) if value.len() == 4 => word(value, 0)?,
            _ => return Err(Error::Firmware),
        };
        if addresses > 2 {
            return Err(Error::Firmware);
        }
        let irq = offset + 20 + addresses as usize * 4;
        let cells = [word(map, irq)?, word(map, irq + 4)?, word(map, irq + 8)?];
        let mut matches = true;
        for (index, key) in key.iter().enumerate() {
            let mask = word(mask, index * 4)?;
            matches &= key & mask == word(map, offset + index * 4)? & mask;
        }
        if matches {
            if cells[0] != 0 || cells[1] >= 988 || cells[2] != 4 || result.is_some() {
                return Err(Error::Interrupt);
            }
            result = Some(cells[1] + 32);
        }
        offset = irq + 12;
    }
    result.ok_or(Error::Interrupt)
}

impl Function {
    pub fn reset(&self) -> bool {
        let mut cap = self.capabilities[0x34 / 4] as usize;
        while cap != 0 {
            let header = self.capabilities[cap / 4];
            if header >> 24 == 1 {
                let bar = (self.capabilities[cap / 4 + 1] & 0xff) as usize;
                let Some(bar) = self.bars.get(bar).copied().flatten() else {
                    return false;
                };
                let offset = self.capabilities[cap / 4 + 2] as usize + 20;
                let registers = Registers(bar.mapping, self.config.1);
                registers.write_width(offset, 1, 0);
                for _ in 0..1024 {
                    if registers.read_width(offset, 1) == 0 {
                        return true;
                    }
                    core::hint::spin_loop();
                }
                return false;
            }
            cap = ((header >> 8) & 0xff) as usize;
        }
        false
    }
    pub fn identity(&self) -> u32 {
        self.config.read(0)
    }
    pub fn state(&self, base: u64, irq: u32) -> FunctionState {
        FunctionState::new(
            self.layouts,
            model::MsixLayout {
                count: 0,
                table: 0,
                pending: 0,
            },
            self.identity(),
            self.config.read(8),
            self.config.read(0x2c),
            base,
            irq,
        )
        .with_transport_config(self.capabilities)
    }
    pub fn line_asserted(&self) -> bool {
        self.config.read(4) & ((1 << 19) | (1 << 10)) == (1 << 19)
    }
    pub fn activate(&self, state: &mut FunctionState) {
        state.activate();
        self.sync(state);
    }
    pub fn stop(&self, state: &mut FunctionState) {
        state.stop();
        self.sync(state);
    }
    fn sync(&self, state: &FunctionState) {
        self.config.write_width(
            4,
            2,
            2 | (u64::from(state.bus_master()) << 2) | (u64::from(!state.intx_enabled()) << 10),
        );
        let _ = self.config.read(4);
    }
    pub fn read(&self, state: &FunctionState, offset: usize, width: usize) -> Option<u64> {
        if offset < super::ECAM_SIZE as usize {
            return state.config_read(offset, width);
        }
        let model::BarAccess::Registers { index, offset } = state.bar_access(offset, width)? else {
            return None;
        };
        Some(Registers(self.bars[index]?.mapping, self.config.1).read_width(offset, width))
    }
    pub fn write(
        &self,
        state: &mut FunctionState,
        offset: usize,
        width: usize,
        value: u64,
    ) -> bool {
        if offset < super::ECAM_SIZE as usize {
            if !state.config_write(offset, width, value) {
                return false;
            }
            if offset & !3 == 4 {
                self.sync(state);
            }
            return true;
        }
        let Some(model::BarAccess::Registers { index, offset }) = state.bar_access(offset, width)
        else {
            return false;
        };
        let Some(bar) = self.bars[index] else {
            return false;
        };
        Registers(bar.mapping, self.config.1).write_width(offset, width, value);
        true
    }
}
