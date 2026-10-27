// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `GICv2` banked Distributor and MMIO CPU interface (non-secure view).

use core::marker::PhantomData;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU32, Ordering};
use hyper_platform::GicV2Info;
use {
    hyper::hal::barrier::Barrier, hyper::hal::barrier::BarrierAccess,
    hyper::hal::barrier::BarrierDomain,
};
use {
    hyper::hal::interrupt::InterruptId, hyper::hal::interrupt::InterruptPriority,
    hyper::hal::interrupt::InterruptTransitionError, hyper::hal::interrupt::InterruptTrigger,
    hyper::hal::interrupt::LocalInterruptController,
};

const ENABLE: usize = 0x100;
const DISABLE: usize = 0x180;
const PRIORITY: usize = 0x400;
const TARGET: usize = 0x800;
const CONFIG: usize = 0xc00;
const SGIR: usize = 0xf00;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidRange,
    InvalidInterrupt,
    InvalidCpuTarget,
}

/// SGI completion must retain the sender bits from IAR. Banked target IDs
/// index CPUs, independently of MPIDR numbering. The GIC cannot acknowledge
/// another instance of an active SGI at the same CPU before its EOI.
pub struct SgiCompletions([[AtomicU32; 16]; 8]);
impl SgiCompletions {
    pub const fn new() -> Self {
        Self([const { [const { AtomicU32::new(0) }; 16] }; 8])
    }
}
impl Default for SgiCompletions {
    fn default() -> Self {
        Self::new()
    }
}

pub struct GicV2<B> {
    local: GicV2Local<B>,
    boot_target: u8,
}

pub struct GicV2Local<B> {
    distributor: usize,
    cpu: usize,
    count: u32,
    completions: &'static SgiCompletions,
    marker: PhantomData<B>,
}
impl<B> Copy for GicV2Local<B> {}
impl<B> Clone for GicV2Local<B> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<B: Barrier> GicV2<B> {
    /// # Safety
    /// The caller owns both complete, permanent Device mappings, and keeps
    /// IRQs masked through initialization. Registers are banked by hardware;
    /// non-banked implementations with cpu-offset are not supported.
    pub unsafe fn bind(
        info: GicV2Info,
        mut map: impl FnMut(u64) -> Option<usize>,
        completions: &'static SgiCompletions,
    ) -> Result<Self, Error> {
        fn mapped(
            range: hyper::mm::ranges::PhysicalRange,
            map: &mut impl FnMut(u64) -> Option<usize>,
        ) -> Result<usize, Error> {
            if range.size() < 0x1000 || !range.start().is_multiple_of(4) {
                return Err(Error::InvalidRange);
            }
            let base = map(range.start()).ok_or(Error::InvalidRange)?;
            if !base.is_multiple_of(4) || base.checked_add(0xfff).is_none() {
                return Err(Error::InvalidRange);
            }
            Ok(base)
        }
        let distributor = mapped(info.distributor, &mut map)?;
        let cpu = mapped(info.cpu_interface, &mut map)?;
        let count = (32 * ((read(distributor, 4) & 31) + 1)).min(1020);
        let local = GicV2Local {
            distributor,
            cpu,
            count,
            completions,
            marker: PhantomData,
        };
        let boot_target = local.target()?;
        Ok(Self { local, boot_target })
    }
    /// # Safety
    /// One-shot distributor initialization, before other CPUs use this GIC.
    pub unsafe fn initialize(&mut self) -> Result<(), Error> {
        let d = self.local.distributor;
        write(d, 0, 0);
        B::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        for n in 1..self.local.count.div_ceil(32) {
            let o = n as usize * 4;
            write(d, 0x80 + o, self.local.group_bits());
            write(d, DISABLE + o, u32::MAX);
            write(d, 0x280 + o, u32::MAX);
            write(d, 0x380 + o, u32::MAX);
        }
        for n in (32..self.local.count).step_by(4) {
            write(d, PRIORITY + n as usize, 0xa0a0a0a0);
            write(
                d,
                TARGET + n as usize,
                u32::from_ne_bytes([self.boot_target; 4]),
            );
        }
        write(d, 0, 1);
        // SAFETY: Boot exclusively owns this masked CPU interface.
        unsafe { self.initialize_local() }
    }
    /// # Safety
    /// Runs on the target CPU with IRQs masked, before local IRQ publication.
    pub unsafe fn initialize_local(&self) -> Result<(), Error> {
        self.local.target()?;
        let d = self.local.distributor;
        write(self.local.cpu, 0, 0);
        write(d, DISABLE, u32::MAX);
        write(d, 0x80, self.local.group_bits());
        write(d, 0x280, u32::MAX);
        write(d, 0x380, u32::MAX);
        for n in 0..4 {
            write(d, 0xf10 + n * 4, u32::MAX);
        }
        for n in 0..8 {
            write(d, PRIORITY + n * 4, 0xa0a0a0a0);
        }
        write(self.local.cpu, 4, 0xff);
        write(self.local.cpu, 8, 0);
        // Enable the NS interface (Group 0 without Security Extensions).
        // EOImode remains clear: one EOIR write also deactivates the IRQ.
        write(self.local.cpu, 0, 1);
        B::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        B::instruction_synchronization();
        Ok(())
    }
    pub fn local_controller(&self) -> GicV2Local<B> {
        self.local
    }
    pub fn interrupt_count(&self) -> u32 {
        self.local.count
    }
    pub fn configure(
        &mut self,
        id: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
    ) -> Result<(), Error> {
        self.local.configure_id(id, priority, trigger)?;
        if id.get() >= 32 {
            write_byte(
                self.local.distributor,
                TARGET + id.get() as usize,
                self.boot_target,
            );
        }
        B::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        Ok(())
    }
    /// Routes a disabled SPI to a previously discovered GIC CPU target bit.
    pub fn route_spi(&mut self, id: InterruptId, target: u8) -> Result<(), Error> {
        if id.get() < 32 || id.get() >= self.local.count || target.count_ones() != 1 {
            return Err(Error::InvalidCpuTarget);
        }
        write_byte(self.local.distributor, TARGET + id.get() as usize, target);
        B::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        Ok(())
    }

    pub fn enable(&mut self, id: InterruptId) -> Result<(), InterruptTransitionError<Error>> {
        self.local.set_enabled(id, true)
    }
    pub fn disable(&mut self, id: InterruptId) -> Result<(), InterruptTransitionError<Error>> {
        self.local.set_enabled(id, false)
    }
}

impl<B: Barrier> GicV2Local<B> {
    fn group_bits(&self) -> u32 {
        // Without Security Extensions the enabled interface is Group 0.
        // In the NS view of a security-enabled GIC, firmware owns IGROUPR;
        // accesses to secure interrupts remain RAZ/WI.
        if read(self.distributor, 4) & (1 << 10) != 0 {
            u32::MAX
        } else {
            0
        }
    }

    pub fn target(&self) -> Result<u8, Error> {
        // Banked SGI/PPI target registers identify this CPU. UP implementations
        // may make ITARGETSR RAZ/WI; only a one-interface GIC permits fallback.
        let value = (0..8).fold(0, |mask, n| mask | read(self.distributor, TARGET + n * 4));
        let mut mask = (value | (value >> 8) | (value >> 16) | (value >> 24)) as u8;
        if mask == 0 && read(self.distributor, 4) & (7 << 5) == 0 {
            mask = 1;
        }
        if mask.count_ones() != 1 {
            return Err(Error::InvalidCpuTarget);
        }
        Ok(mask)
    }
    pub fn send_sgi(&self, id: InterruptId, target: u8) -> bool {
        if id.get() >= 16 || target == 0 {
            return false;
        }
        B::data_synchronization(BarrierDomain::InnerShareable, BarrierAccess::Writes);
        write(self.distributor, SGIR, (u32::from(target) << 16) | id.get());
        true
    }
    pub fn broadcast_sgi(&self, id: InterruptId) -> bool {
        if id.get() >= 16 {
            return false;
        }
        B::data_synchronization(BarrierDomain::InnerShareable, BarrierAccess::Writes);
        write(self.distributor, SGIR, (1 << 24) | id.get());
        true
    }
    pub fn acknowledge(&self) -> Option<InterruptId> {
        let raw = read(self.cpu, 0xc);
        let id = raw & 0x3ff;
        if id >= 1020 {
            return None;
        }
        if id < 16 {
            let cpu = self.target().ok()?.trailing_zeros() as usize;
            self.completions.0[cpu][id as usize].store(raw, Ordering::Relaxed);
        }
        Some(InterruptId::new(id))
    }
    pub fn end(&self, id: InterruptId) {
        let id = id.get();
        if id >= self.count {
            return;
        }
        let raw = if id < 16 {
            let Ok(mask) = self.target() else {
                return;
            };
            self.completions.0[mask.trailing_zeros() as usize][id as usize].load(Ordering::Relaxed)
        } else {
            id
        };
        B::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        write(self.cpu, 0x10, raw);
        B::instruction_synchronization();
    }
    fn configure_id(
        &self,
        id: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
    ) -> Result<(), Error> {
        let id = id.get();
        if id >= self.count || (id < 16 && trigger != InterruptTrigger::Edge) {
            return Err(Error::InvalidInterrupt);
        }
        let priority = match priority {
            InterruptPriority::Critical => 0,
            InterruptPriority::High => 0x40,
            InterruptPriority::Normal => 0x80,
            InterruptPriority::Low => 0xc0,
        };
        write_byte(self.distributor, PRIORITY + id as usize, priority);
        if id >= 16 {
            let offset = CONFIG + (id / 16) as usize * 4;
            let bit = 1 << ((id % 16) * 2 + 1);
            let value = read(self.distributor, offset) & !bit;
            write(
                self.distributor,
                offset,
                value
                    | if trigger == InterruptTrigger::Edge {
                        bit
                    } else {
                        0
                    },
            );
        }
        B::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        Ok(())
    }
    fn set_enabled(
        &self,
        id: InterruptId,
        enabled: bool,
    ) -> Result<(), InterruptTransitionError<Error>> {
        let id = id.get();
        if id >= self.count {
            return Err(InterruptTransitionError::NotApplied(
                Error::InvalidInterrupt,
            ));
        }
        write(
            self.distributor,
            (if enabled { ENABLE } else { DISABLE }) + (id / 32) as usize * 4,
            1 << (id % 32),
        );
        B::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        Ok(())
    }
}
impl<B: Barrier> LocalInterruptController for GicV2Local<B> {
    type Error = Error;
    fn configure(
        &self,
        id: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
    ) -> Result<(), Error> {
        if id.get() >= 32 {
            return Err(Error::InvalidInterrupt);
        }
        self.configure_id(id, priority, trigger)
    }
    fn enable(&self, id: InterruptId) -> Result<(), InterruptTransitionError<Error>> {
        if id.get() >= 32 {
            return Err(InterruptTransitionError::NotApplied(
                Error::InvalidInterrupt,
            ));
        }
        self.set_enabled(id, true)
    }
    fn disable(&self, id: InterruptId) -> Result<(), InterruptTransitionError<Error>> {
        if id.get() >= 32 {
            return Err(InterruptTransitionError::NotApplied(
                Error::InvalidInterrupt,
            ));
        }
        self.set_enabled(id, false)
    }
}
fn read(base: usize, offset: usize) -> u32 {
    // SAFETY: bind validates the persistent aligned 4 KiB mapping; all offsets
    // are aligned and bounded by the architectural interrupt limit (1020).
    unsafe { read_volatile((base + offset) as *const u32) }
}
fn write(base: usize, offset: usize, value: u32) {
    // SAFETY: Same validated register mapping and bounds as read().
    unsafe { write_volatile((base + offset) as *mut u32, value) }
}
fn write_byte(base: usize, offset: usize, value: u8) {
    // SAFETY: GIC priority/target byte registers lie within the owned mapping.
    unsafe { write_volatile((base + offset) as *mut u8, value) }
}
