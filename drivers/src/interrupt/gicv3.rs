// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::marker::PhantomData;
use core::ptr::{read_volatile, write_volatile};

use {
    hyper::hal::barrier::Barrier, hyper::hal::barrier::BarrierAccess,
    hyper::hal::barrier::BarrierDomain,
};
use {
    hyper::hal::interrupt::InterruptController, hyper::hal::interrupt::InterruptId,
    hyper::hal::interrupt::InterruptPriority, hyper::hal::interrupt::InterruptTransitionError,
    hyper::hal::interrupt::InterruptTrigger, hyper::hal::interrupt::LocalInterruptController,
};
use {
    hyper::hal::timer::MonotonicCounter, hyper::hal::timer::deadline_reached,
    hyper::hal::timer::nanoseconds_to_ticks,
};
use {hyper_platform::GicV3Info, hyper_platform::MAX_GIC_REDISTRIBUTOR_REGIONS};

const DISTRIBUTOR_MIN_SIZE: u64 = 0x1_0000;
const REDISTRIBUTOR_FRAME_SIZE: u64 = 0x2_0000;
const SGI_BASE_OFFSET: usize = 0x1_0000;
const DEFAULT_REDISTRIBUTOR_STRIDE: u64 = REDISTRIBUTOR_FRAME_SIZE;
const REGISTER_POLL_TIMEOUT_NS: u64 = 1_000_000_000;

const GICD_CTLR: usize = 0x0000;
const GICD_TYPER: usize = 0x0004;
const GICD_IGROUPR: usize = 0x0080;
const GICD_ISENABLER: usize = 0x0100;
const GICD_ICENABLER: usize = 0x0180;
const GICD_ICPENDR: usize = 0x0280;
const GICD_ICACTIVER: usize = 0x0380;
const GICD_IPRIORITYR: usize = 0x0400;
const GICD_ICFGR: usize = 0x0c00;
const GICD_IROUTER: usize = 0x6000;

const GICR_CTLR: usize = 0x0000;
const GICR_TYPER: usize = 0x0008;
const GICR_WAKER: usize = 0x0014;

const fn gic_priority(priority: InterruptPriority) -> u8 {
    match priority {
        InterruptPriority::Critical => 0x00,
        InterruptPriority::High => 0x40,
        InterruptPriority::Normal => 0x80,
        InterruptPriority::Low => 0xc0,
    }
}
const GICR_IGROUPR0: usize = SGI_BASE_OFFSET + 0x0080;
const GICR_ICENABLER0: usize = SGI_BASE_OFFSET + 0x0180;
const GICR_ICPENDR0: usize = SGI_BASE_OFFSET + 0x0280;
const GICR_ICACTIVER0: usize = SGI_BASE_OFFSET + 0x0380;
const GICR_IPRIORITYR: usize = SGI_BASE_OFFSET + 0x0400;

const GICD_CTLR_ENABLE_GROUP1: u32 = 1 << 0;
const GICD_CTLR_ENABLE_GROUP1_AFFINITY: u32 = 1 << 1;
// Non-secure GICD_CTLR view: ARE_NS is bit 4. Bit 5 belongs to the
// security-enabled register view and must not be used by the EL2 kernel.
const GICD_CTLR_ARE_NONSECURE: u32 = 1 << 4;
const GICD_CTLR_RWP: u32 = 1 << 31;
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const GICR_TYPER_LAST: u64 = 1 << 4;
const GICR_CTLR_RWP: u32 = 1 << 3;
const SPURIOUS_INTERRUPT_MIN: u32 = 1020;
const DEFAULT_PRIORITY: u8 = 0xa0;

/// Architecture bridge for the `GICv3` system-register CPU interface.
pub trait CpuInterface {
    /// Enables the local GIC system-register interface for the current CPU.
    ///
    /// # Safety
    ///
    /// The caller must execute at an exception level allowed to configure the
    /// physical GIC CPU interface, with local interrupts masked.
    unsafe fn initialize() -> bool;

    fn acknowledge() -> u32;
    fn end(interrupt: u32);

    fn affinity() -> u32;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AddressOverflow,
    DistributorTooSmall,
    InvalidInterrupt,
    InvalidRedistributorStride,
    NoMatchingRedistributor,
    PollingClockUnavailable,
    RegisterTimeout,
    SystemRegisterInterfaceUnavailable,
}

#[derive(Clone, Copy)]
struct MappedRegion {
    base: usize,
    size: u64,
}

impl MappedRegion {
    const EMPTY: Self = Self { base: 0, size: 0 };
}

/// `GICv3` Distributor and per-CPU Redistributor state.
///
/// `Counter` supplies bounded register handshakes before kernel timekeeping is
/// initialized; the architecture binding selects the available early counter.
pub struct GicV3<Cpu, MemoryBarrier, Counter> {
    distributor: MappedRegion,
    redistributors: [MappedRegion; MAX_GIC_REDISTRIBUTOR_REGIONS],
    redistributor_count: usize,
    redistributor_stride: u64,
    boot_affinity: u32,
    interrupt_count: u32,
    marker: PhantomData<(Cpu, MemoryBarrier, Counter)>,
}

#[derive(Clone, Copy)]
pub struct GicV3Local<Cpu, MemoryBarrier, Counter> {
    redistributor: usize,
    interrupt_count: u32,
    marker: PhantomData<(Cpu, MemoryBarrier, Counter)>,
}

impl<Cpu: CpuInterface, MemoryBarrier: Barrier, Counter: MonotonicCounter> LocalInterruptController
    for GicV3Local<Cpu, MemoryBarrier, Counter>
{
    type Error = Error;

    fn configure(
        &self,
        interrupt: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
    ) -> Result<(), Error> {
        let id = self.validate(interrupt)?;
        if id < 16 && trigger != InterruptTrigger::Edge {
            return Err(Error::InvalidInterrupt);
        }
        let base = self
            .redistributor
            .checked_add(SGI_BASE_OFFSET)
            .ok_or(Error::AddressOverflow)?;
        write_u8(base, GICD_IPRIORITYR + id as usize, gic_priority(priority));
        if id >= 16 {
            let offset = GICD_ICFGR + ((id / 16) as usize * 4);
            let bit = 1u32 << (((id % 16) * 2) + 1);
            let mut value = read_u32(base, offset);
            match trigger {
                InterruptTrigger::Level => value &= !bit,
                InterruptTrigger::Edge => value |= bit,
            }
            write_u32(base, offset, value);
        }
        self.finish()
    }

    fn enable(&self, interrupt: InterruptId) -> Result<(), InterruptTransitionError<Error>> {
        self.set_enabled(interrupt, true)
    }

    fn disable(&self, interrupt: InterruptId) -> Result<(), InterruptTransitionError<Error>> {
        self.set_enabled(interrupt, false)
    }
}

impl<Cpu: CpuInterface, MemoryBarrier: Barrier, Counter: MonotonicCounter>
    GicV3Local<Cpu, MemoryBarrier, Counter>
{
    fn validate(&self, interrupt: InterruptId) -> Result<u32, Error> {
        let id = interrupt.get();
        (id < 32 && id < self.interrupt_count)
            .then_some(id)
            .ok_or(Error::InvalidInterrupt)
    }

    fn set_enabled(
        &self,
        interrupt: InterruptId,
        enabled: bool,
    ) -> Result<(), InterruptTransitionError<Error>> {
        let id = self
            .validate(interrupt)
            .map_err(InterruptTransitionError::NotApplied)?;
        let base = self
            .redistributor
            .checked_add(SGI_BASE_OFFSET)
            .ok_or(InterruptTransitionError::NotApplied(Error::AddressOverflow))?;
        write_u32(
            base,
            if enabled {
                GICD_ISENABLER
            } else {
                GICD_ICENABLER
            },
            1 << id,
        );
        self.finish()
            .map_err(InterruptTransitionError::AppliedOrUnknown)
    }

    fn finish(&self) -> Result<(), Error> {
        wait_for_register::<Counter>(|| {
            read_u32(self.redistributor, GICR_CTLR) & GICR_CTLR_RWP == 0
        })?;
        MemoryBarrier::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        Ok(())
    }
}

impl<Cpu: CpuInterface, MemoryBarrier: Barrier, Counter: MonotonicCounter>
    GicV3<Cpu, MemoryBarrier, Counter>
{
    /// Binds DTB-discovered register ranges to architecture-provided mappings.
    ///
    /// # Safety
    ///
    /// `map` must return Device-memory mappings for the complete physical
    /// ranges supplied to it. Each returned range must remain exclusively
    /// owned by this driver for its lifetime.
    pub unsafe fn bind(
        info: GicV3Info,
        mut map: impl FnMut(u64) -> Option<usize>,
    ) -> Result<Self, Error> {
        if info.distributor.size() < DISTRIBUTOR_MIN_SIZE {
            return Err(Error::DistributorTooSmall);
        }
        let stride = match info.redistributor_stride {
            Some(stride) => stride,
            None => DEFAULT_REDISTRIBUTOR_STRIDE,
        };
        if stride < REDISTRIBUTOR_FRAME_SIZE || !stride.is_multiple_of(0x1_0000) {
            return Err(Error::InvalidRedistributorStride);
        }
        let distributor = MappedRegion {
            base: map(info.distributor.start()).ok_or(Error::AddressOverflow)?,
            size: info.distributor.size(),
        };
        let mut redistributors = [MappedRegion::EMPTY; MAX_GIC_REDISTRIBUTOR_REGIONS];
        let regions = info.redistributors.as_slice();
        for (slot, region) in redistributors.iter_mut().zip(regions) {
            *slot = MappedRegion {
                base: map(region.start()).ok_or(Error::AddressOverflow)?,
                size: region.size(),
            };
        }
        Ok(Self {
            distributor,
            redistributors,
            redistributor_count: regions.len(),
            redistributor_stride: stride,
            boot_affinity: 0,
            interrupt_count: 0,
            marker: PhantomData,
        })
    }

    /// Initializes the shared Distributor and the boot CPU's Redistributor.
    ///
    /// # Safety
    ///
    /// This must run once with local interrupts masked before another CPU or
    /// driver accesses the interrupt controller.
    pub unsafe fn initialize(&mut self, affinity: u32) -> Result<(), Error> {
        self.boot_affinity = affinity;
        self.initialize_distributor()?;
        // SAFETY: This method's contract keeps local IRQs masked and completes
        // shared Distributor setup before local interface initialization.
        unsafe { self.initialize_local(affinity) }
    }

    /// Initializes the calling CPU's Redistributor and CPU interface.
    ///
    /// # Safety
    ///
    /// The shared Distributor must already be initialized and local IRQs must
    /// remain masked throughout this operation.
    pub unsafe fn initialize_local(&self, affinity: u32) -> Result<(), Error> {
        let redistributor = self.find_redistributor(affinity)?;
        self.initialize_redistributor(redistributor)?;
        MemoryBarrier::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        // SAFETY: The matching Redistributor is awake, shared initialization
        // is complete, and the caller keeps local interrupts masked.
        if !unsafe { Cpu::initialize() } {
            return Err(Error::SystemRegisterInterfaceUnavailable);
        }
        Ok(())
    }

    pub const fn interrupt_count(&self) -> u32 {
        self.interrupt_count
    }

    pub fn local_controller(&self) -> Result<GicV3Local<Cpu, MemoryBarrier, Counter>, Error> {
        Ok(GicV3Local {
            redistributor: self.current_redistributor()?,
            interrupt_count: self.interrupt_count,
            marker: PhantomData,
        })
    }

    pub fn configure(
        &mut self,
        interrupt: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
    ) -> Result<(), Error> {
        let id = self.validate_configurable(interrupt)?;
        if id < 16 && trigger != InterruptTrigger::Edge {
            return Err(Error::InvalidInterrupt);
        }
        let (base, local_id, redistributor) = if id < 32 {
            let redistributor = self.current_redistributor()?;
            (
                redistributor
                    .checked_add(SGI_BASE_OFFSET)
                    .ok_or(Error::AddressOverflow)?,
                id,
                Some(redistributor),
            )
        } else {
            (self.distributor.base, id, None)
        };
        write_u8(
            base,
            GICD_IPRIORITYR + local_id as usize,
            gic_priority(priority),
        );

        if id >= 16 {
            let configuration_offset = GICD_ICFGR + ((local_id / 16) as usize * 4);
            let trigger_bit = 1u32 << (((local_id % 16) * 2) + 1);
            let mut configuration = read_u32(base, configuration_offset);
            match trigger {
                InterruptTrigger::Level => configuration &= !trigger_bit,
                InterruptTrigger::Edge => configuration |= trigger_bit,
            }
            write_u32(base, configuration_offset, configuration);
        }
        if id >= 32 {
            write_u64(
                self.distributor.base,
                GICD_IROUTER + ((id as usize) * 8),
                u64::from(self.boot_affinity),
            );
        }
        self.wait_for_write(redistributor)?;
        MemoryBarrier::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        Ok(())
    }

    /// Routes a disabled SPI to one affinity, without enabling 1-of-N delivery.
    pub fn route_spi(&mut self, interrupt: InterruptId, affinity: u32) -> Result<(), Error> {
        let id = self.validate_configurable(interrupt)?;
        if id < 32 {
            return Err(Error::InvalidInterrupt);
        }
        let route = u64::from(affinity & 0x00ff_ffff) | (u64::from(affinity >> 24) << 32);
        write_u64(self.distributor.base, GICD_IROUTER + id as usize * 8, route);
        self.wait_for_write(None)?;
        MemoryBarrier::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        Ok(())
    }

    fn initialize_distributor(&mut self) -> Result<(), Error> {
        write_u32(self.distributor.base, GICD_CTLR, 0);
        self.wait_for_distributor_write()?;

        let typer = read_u32(self.distributor.base, GICD_TYPER);
        self.interrupt_count = (32 * ((typer & 0x1f) + 1)).min(SPURIOUS_INTERRUPT_MIN);
        let register_count = self.interrupt_count.div_ceil(32);
        for register in 1..register_count {
            let offset = register as usize * 4;
            write_u32(self.distributor.base, GICD_IGROUPR + offset, u32::MAX);
            write_u32(self.distributor.base, GICD_ICENABLER + offset, u32::MAX);
            write_u32(self.distributor.base, GICD_ICPENDR + offset, u32::MAX);
            write_u32(self.distributor.base, GICD_ICACTIVER + offset, u32::MAX);
        }
        for register in 8..self.interrupt_count.div_ceil(4) {
            write_u32(
                self.distributor.base,
                GICD_IPRIORITYR + (register as usize * 4),
                u32::from_ne_bytes([DEFAULT_PRIORITY; 4]),
            );
        }

        write_u32(
            self.distributor.base,
            GICD_CTLR,
            GICD_CTLR_ARE_NONSECURE | GICD_CTLR_ENABLE_GROUP1 | GICD_CTLR_ENABLE_GROUP1_AFFINITY,
        );
        self.wait_for_distributor_write()
    }

    fn initialize_redistributor(&self, redistributor: usize) -> Result<(), Error> {
        let mut waker = read_u32(redistributor, GICR_WAKER);
        waker &= !GICR_WAKER_PROCESSOR_SLEEP;
        write_u32(redistributor, GICR_WAKER, waker);
        wait_for_register::<Counter>(|| {
            read_u32(redistributor, GICR_WAKER) & GICR_WAKER_CHILDREN_ASLEEP == 0
        })?;
        write_u32(redistributor, GICR_IGROUPR0, u32::MAX);
        write_u32(redistributor, GICR_ICENABLER0, u32::MAX);
        write_u32(redistributor, GICR_ICPENDR0, u32::MAX);
        write_u32(redistributor, GICR_ICACTIVER0, u32::MAX);
        for register in 0..8 {
            write_u32(
                redistributor,
                GICR_IPRIORITYR + register * 4,
                u32::from_ne_bytes([DEFAULT_PRIORITY; 4]),
            );
        }
        self.wait_for_redistributor_write(redistributor)
    }

    fn find_redistributor(&self, affinity: u32) -> Result<usize, Error> {
        for region in &self.redistributors[..self.redistributor_count] {
            let mut offset = 0u64;
            while offset
                .checked_add(REDISTRIBUTOR_FRAME_SIZE)
                .filter(|end| *end <= region.size)
                .is_some()
            {
                let base = region
                    .base
                    .checked_add(usize::try_from(offset).map_err(|_| Error::AddressOverflow)?)
                    .ok_or(Error::AddressOverflow)?;
                let typer = read_u64(base, GICR_TYPER);
                if (typer >> 32) as u32 == affinity {
                    return Ok(base);
                }
                if typer & GICR_TYPER_LAST != 0 {
                    break;
                }
                offset = offset
                    .checked_add(self.redistributor_stride)
                    .ok_or(Error::AddressOverflow)?;
            }
        }
        Err(Error::NoMatchingRedistributor)
    }

    fn current_redistributor(&self) -> Result<usize, Error> {
        self.find_redistributor(Cpu::affinity())
    }

    fn wait_for_distributor_write(&self) -> Result<(), Error> {
        wait_for_register::<Counter>(|| {
            read_u32(self.distributor.base, GICD_CTLR) & GICD_CTLR_RWP == 0
        })
    }

    fn wait_for_redistributor_write(&self, redistributor: usize) -> Result<(), Error> {
        wait_for_register::<Counter>(|| read_u32(redistributor, GICR_CTLR) & GICR_CTLR_RWP == 0)
    }

    fn wait_for_write(&self, redistributor: Option<usize>) -> Result<(), Error> {
        match redistributor {
            Some(base) => self.wait_for_redistributor_write(base),
            None => self.wait_for_distributor_write(),
        }
    }

    fn validate_configurable(&self, interrupt: InterruptId) -> Result<u32, Error> {
        let id = interrupt.get();
        if id >= self.interrupt_count {
            return Err(Error::InvalidInterrupt);
        }
        Ok(id)
    }
}

impl<Cpu: CpuInterface, MemoryBarrier: Barrier, Counter: MonotonicCounter> InterruptController
    for GicV3<Cpu, MemoryBarrier, Counter>
{
    type Error = Error;

    fn enable(
        &mut self,
        interrupt: InterruptId,
    ) -> Result<(), InterruptTransitionError<Self::Error>> {
        let id = self
            .validate_configurable(interrupt)
            .map_err(InterruptTransitionError::NotApplied)?;
        let (base, local_id, redistributor) = if id < 32 {
            let redistributor = self
                .current_redistributor()
                .map_err(InterruptTransitionError::NotApplied)?;
            (
                redistributor
                    .checked_add(SGI_BASE_OFFSET)
                    .ok_or(InterruptTransitionError::NotApplied(Error::AddressOverflow))?,
                id,
                Some(redistributor),
            )
        } else {
            (self.distributor.base, id, None)
        };
        let offset = (local_id / 32) as usize * 4;
        write_u32(base, GICD_ISENABLER + offset, 1 << (local_id % 32));
        self.wait_for_write(redistributor)
            .map_err(InterruptTransitionError::AppliedOrUnknown)?;
        MemoryBarrier::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        Ok(())
    }

    fn disable(
        &mut self,
        interrupt: InterruptId,
    ) -> Result<(), InterruptTransitionError<Self::Error>> {
        let id = self
            .validate_configurable(interrupt)
            .map_err(InterruptTransitionError::NotApplied)?;
        let (base, local_id, redistributor) = if id < 32 {
            let redistributor = self
                .current_redistributor()
                .map_err(InterruptTransitionError::NotApplied)?;
            (
                redistributor
                    .checked_add(SGI_BASE_OFFSET)
                    .ok_or(InterruptTransitionError::NotApplied(Error::AddressOverflow))?,
                id,
                Some(redistributor),
            )
        } else {
            (self.distributor.base, id, None)
        };
        let offset = (local_id / 32) as usize * 4;
        write_u32(base, GICD_ICENABLER + offset, 1 << (local_id % 32));
        self.wait_for_write(redistributor)
            .map_err(InterruptTransitionError::AppliedOrUnknown)?;
        MemoryBarrier::data_synchronization(BarrierDomain::FullSystem, BarrierAccess::All);
        Ok(())
    }

    fn acknowledge(&self) -> Option<InterruptId> {
        let raw = Cpu::acknowledge();
        (raw < SPURIOUS_INTERRUPT_MIN).then_some(InterruptId::new(raw))
    }

    fn end(&self, interrupt: InterruptId) {
        Cpu::end(interrupt.get());
    }
}

fn wait_for_register<Counter: MonotonicCounter>(
    mut complete: impl FnMut() -> bool,
) -> Result<(), Error> {
    if complete() {
        return Ok(());
    }
    let frequency = Counter::frequency_hz().map_err(|_| Error::PollingClockUnavailable)?;
    let timeout = nanoseconds_to_ticks(REGISTER_POLL_TIMEOUT_NS, frequency)
        .map_err(|_| Error::PollingClockUnavailable)?;
    if timeout > i64::MAX as u64 {
        return Err(Error::PollingClockUnavailable);
    }
    let deadline = Counter::read().wrapping_add(timeout);
    loop {
        if complete() {
            return Ok(());
        }
        if deadline_reached(Counter::read(), deadline) {
            return Err(Error::RegisterTimeout);
        }
        core::hint::spin_loop();
    }
}

fn read_u32(base: usize, offset: usize) -> u32 {
    // SAFETY: GicV3 construction validates and retains the complete mapped
    // register resource; every caller uses a specified aligned u32 offset.
    unsafe { read_volatile(base.wrapping_add(offset) as *const u32) }
}

fn read_u64(base: usize, offset: usize) -> u64 {
    // SAFETY: Redistributor bases and GICR_TYPER are eight-byte aligned inside
    // the validated persistent register mapping.
    unsafe { read_volatile(base.wrapping_add(offset) as *const u64) }
}

fn write_u8(base: usize, offset: usize, value: u8) {
    // SAFETY: The byte priority register lies inside the owned GIC mapping.
    unsafe { write_volatile(base.wrapping_add(offset) as *mut u8, value) };
}

fn write_u32(base: usize, offset: usize, value: u32) {
    // SAFETY: Callers use architecturally aligned u32 registers within the
    // constructor-validated owned GIC resource.
    unsafe { write_volatile(base.wrapping_add(offset) as *mut u32, value) };
}

fn write_u64(base: usize, offset: usize, value: u64) {
    // SAFETY: IROUTER registers are aligned u64 locations in the validated
    // permanent Distributor mapping.
    unsafe { write_volatile(base.wrapping_add(offset) as *mut u64, value) };
}
