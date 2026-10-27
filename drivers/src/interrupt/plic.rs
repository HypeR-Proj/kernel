// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Platform-Level Interrupt Controller for supervisor-mode RISC-V kernels.

use core::marker::PhantomData;
use core::ptr::{read_volatile, write_volatile};

use {
    hyper::hal::barrier::Barrier, hyper::hal::barrier::BarrierAccess,
    hyper::hal::barrier::BarrierDomain,
};
use {
    hyper::hal::interrupt::InterruptController, hyper::hal::interrupt::InterruptId,
    hyper::hal::interrupt::InterruptPriority, hyper::hal::interrupt::InterruptTransitionError,
    hyper::hal::interrupt::InterruptTrigger, hyper::hal::interrupt::KernelInterruptController,
    hyper::hal::interrupt::LocalInterruptController,
};
use {hyper_platform::MAX_PLIC_CONTEXTS, hyper_platform::PlicInfo};

const TIMER_INTERRUPT: u32 = 0;
const PRIORITY_BASE: usize = 0;
const ENABLE_BASE: usize = 0x2000;
const ENABLE_STRIDE: usize = 0x80;
const CONTEXT_BASE: usize = 0x20_0000;
const CONTEXT_STRIDE: usize = 0x1000;
const CONTEXT_THRESHOLD: usize = 0;
const CONTEXT_CLAIM: usize = 4;

const fn plic_priority(priority: InterruptPriority) -> u32 {
    match priority {
        InterruptPriority::Critical => 7,
        InterruptPriority::High => 5,
        InterruptPriority::Normal => 3,
        InterruptPriority::Low => 1,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AddressOverflow,
    InvalidContext,
    InvalidInterrupt,
    InvalidRegisterRange,
    UnsupportedTrigger,
}

pub struct Plic<B: Barrier> {
    base: usize,
    source_count: u32,
    contexts: [u32; MAX_PLIC_CONTEXTS],
    context_count: usize,
    hardware_id: fn() -> u64,
    barrier: PhantomData<B>,
}

pub struct PlicLocal<B: Barrier>(PhantomData<B>);

impl<B: Barrier> LocalInterruptController for PlicLocal<B> {
    type Error = Error;

    fn configure(
        &self,
        interrupt: InterruptId,
        _priority: InterruptPriority,
        _trigger: InterruptTrigger,
    ) -> Result<(), Error> {
        (interrupt.get() == TIMER_INTERRUPT)
            .then_some(())
            .ok_or(Error::InvalidInterrupt)
    }

    fn enable(&self, interrupt: InterruptId) -> Result<(), InterruptTransitionError<Error>> {
        self.configure(
            interrupt,
            InterruptPriority::Normal,
            InterruptTrigger::Level,
        )
        .map_err(InterruptTransitionError::NotApplied)
    }

    fn disable(&self, interrupt: InterruptId) -> Result<(), InterruptTransitionError<Error>> {
        self.configure(
            interrupt,
            InterruptPriority::Normal,
            InterruptTrigger::Level,
        )
        .map_err(InterruptTransitionError::NotApplied)
    }
}

impl<B: Barrier> Plic<B> {
    /// Binds a PLIC to one permanent MMIO mapping.
    ///
    /// # Safety
    ///
    /// `map` must return a mapping for the complete DT-described register
    /// range, and the PLIC must have a single driver owner.
    pub unsafe fn bind(
        info: PlicInfo,
        mut map: impl FnMut(u64) -> Option<usize>,
        hardware_id: fn() -> u64,
    ) -> Result<Self, Error> {
        validate_register_range(info)?;
        let base = map(info.registers.start()).ok_or(Error::AddressOverflow)?;
        let mut controller = Self {
            base,
            source_count: info.source_count,
            contexts: info.supervisor_contexts,
            context_count: info.context_count,
            hardware_id,
            barrier: PhantomData,
        };
        // SAFETY: bind validated the full register range and the caller keeps
        // this controller exclusively owned while local state is initialized.
        unsafe { controller.initialize_local()? };
        Ok(controller)
    }

    fn context(&self) -> Result<usize, Error> {
        let hardware_id =
            usize::try_from((self.hardware_id)()).map_err(|_| Error::InvalidContext)?;
        if hardware_id >= self.context_count {
            return Err(Error::InvalidContext);
        }
        let context = *self
            .contexts
            .get(hardware_id)
            .ok_or(Error::InvalidContext)?;
        self.base
            .checked_add(CONTEXT_BASE + context as usize * CONTEXT_STRIDE)
            .ok_or(Error::AddressOverflow)
    }

    fn validate(&self, interrupt: InterruptId) -> Result<u32, Error> {
        let source = interrupt.get();
        if source > self.source_count {
            Err(Error::InvalidInterrupt)
        } else {
            Ok(source)
        }
    }

    fn update_enable(&self, source: u32, enabled: bool) -> Result<(), Error> {
        if source == TIMER_INTERRUPT {
            return Ok(());
        }
        let hardware_id =
            usize::try_from((self.hardware_id)()).map_err(|_| Error::InvalidContext)?;
        if hardware_id >= self.context_count {
            return Err(Error::InvalidContext);
        }
        let context = *self
            .contexts
            .get(hardware_id)
            .ok_or(Error::InvalidContext)? as usize;
        let word = source as usize / 32;
        let register = self
            .base
            .checked_add(ENABLE_BASE + context * ENABLE_STRIDE + word * 4)
            .ok_or(Error::AddressOverflow)? as *mut u32;
        let mut value = read_mmio::<B>(register);
        if enabled {
            value |= 1 << (source % 32);
        } else {
            value &= !(1 << (source % 32));
        }
        write_control_mmio::<B>(register, value);
        Ok(())
    }
}

fn validate_register_range(info: PlicInfo) -> Result<(), Error> {
    if info.source_count == 0 || info.context_count == 0 || info.context_count > MAX_PLIC_CONTEXTS {
        return Err(Error::InvalidRegisterRange);
    }
    let largest_context = info.supervisor_contexts[..info.context_count]
        .iter()
        .copied()
        .max()
        .ok_or(Error::InvalidRegisterRange)? as u64;
    let priority_end = u64::from(info.source_count)
        .checked_mul(4)
        .and_then(|offset| offset.checked_add(4))
        .ok_or(Error::AddressOverflow)?;
    let enable_end = u64::try_from(ENABLE_BASE)
        .ok()
        .and_then(|base| {
            largest_context
                .checked_mul(ENABLE_STRIDE as u64)
                .and_then(|v| base.checked_add(v))
        })
        .and_then(|offset| offset.checked_add(u64::from(info.source_count / 32) * 4))
        .and_then(|offset| offset.checked_add(4))
        .ok_or(Error::AddressOverflow)?;
    let context_end = u64::try_from(CONTEXT_BASE)
        .ok()
        .and_then(|base| {
            largest_context
                .checked_mul(CONTEXT_STRIDE as u64)
                .and_then(|v| base.checked_add(v))
        })
        .and_then(|offset| offset.checked_add((CONTEXT_CLAIM + 4) as u64))
        .ok_or(Error::AddressOverflow)?;
    if priority_end.max(enable_end).max(context_end) > info.registers.size() {
        return Err(Error::InvalidRegisterRange);
    }
    Ok(())
}

impl<B: Barrier> InterruptController for Plic<B> {
    type Error = Error;

    fn enable(
        &mut self,
        interrupt: InterruptId,
    ) -> Result<(), InterruptTransitionError<Self::Error>> {
        let source = self
            .validate(interrupt)
            .map_err(InterruptTransitionError::NotApplied)?;
        self.update_enable(source, true)
            .map_err(InterruptTransitionError::NotApplied)
    }

    fn disable(
        &mut self,
        interrupt: InterruptId,
    ) -> Result<(), InterruptTransitionError<Self::Error>> {
        let source = self
            .validate(interrupt)
            .map_err(InterruptTransitionError::NotApplied)?;
        self.update_enable(source, false)
            .map_err(InterruptTransitionError::NotApplied)
    }

    fn acknowledge(&self) -> Option<InterruptId> {
        let context = self.context().ok()?;
        let source = read_mmio::<B>((context + CONTEXT_CLAIM) as *const u32);
        (source != 0 && source <= self.source_count).then_some(InterruptId::new(source))
    }

    fn end(&self, interrupt: InterruptId) {
        if interrupt.get() == TIMER_INTERRUPT {
            return;
        }
        if let Ok(context) = self.context() {
            write_completion_mmio::<B>((context + CONTEXT_CLAIM) as *mut u32, interrupt.get());
        }
    }
}

impl<B: Barrier> KernelInterruptController for Plic<B> {
    type Local = PlicLocal<B>;

    fn interrupt_count(&self) -> u32 {
        self.source_count.saturating_add(1)
    }

    fn configure(
        &mut self,
        interrupt: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
    ) -> Result<(), Self::Error> {
        let source = self.validate(interrupt)?;
        if source == TIMER_INTERRUPT {
            return Ok(());
        }
        if trigger != InterruptTrigger::Level {
            return Err(Error::UnsupportedTrigger);
        }
        write_control_mmio::<B>(
            (self.base + PRIORITY_BASE + source as usize * 4) as *mut u32,
            plic_priority(priority),
        );
        Ok(())
    }

    fn is_per_cpu(&self, interrupt: InterruptId) -> bool {
        interrupt.get() == TIMER_INTERRUPT
    }

    fn local_controller(&self) -> Result<Self::Local, Self::Error> {
        self.context()?;
        Ok(PlicLocal(PhantomData))
    }

    unsafe fn initialize_local(&mut self) -> Result<Self::Local, Self::Error> {
        let context = self.context()?;
        write_control_mmio::<B>((context + CONTEXT_THRESHOLD) as *mut u32, 0);
        Ok(PlicLocal(PhantomData))
    }
}

fn read_mmio<B: Barrier>(register: *const u32) -> u32 {
    // Order a preceding completion/configuration access before a later claim
    // or read-modify-write. PLIC mappings may occupy the RISC-V I/O domain,
    // so an ordinary atomic acquire is not a substitute for this boundary.
    B::data_memory(BarrierDomain::FullSystem, BarrierAccess::All);
    // SAFETY: Plic validates the DT register extent at binding and every
    // caller derives an aligned register pointer within that owned mapping.
    let value = unsafe { read_volatile(register) };
    // Only the completed input/read must precede following accesses. A load
    // barrier is sufficient; a full read/write fence would over-serialize the
    // hart after every interrupt claim.
    B::data_memory(BarrierDomain::FullSystem, BarrierAccess::Reads);
    value
}

fn write_control_mmio<B: Barrier>(register: *mut u32, value: u32) {
    write_mmio::<B>(register, value, BarrierAccess::Writes);
}

fn write_completion_mmio<B: Barrier>(register: *mut u32, value: u32) {
    write_mmio::<B>(register, value, BarrierAccess::All);
}

fn write_mmio<B: Barrier>(register: *mut u32, value: u32, preceding: BarrierAccess) {
    // PLIC completion may follow status/data reads from the interrupting
    // device, while configuration and enable writes need only order earlier
    // memory writes/device outputs. The closed wrappers above select the
    // weakest correct predecessor set for each operation.
    B::data_memory(BarrierDomain::FullSystem, preceding);
    // SAFETY: The pointer is an aligned writable PLIC register derived from
    // the constructor-validated permanent mapping.
    unsafe { write_volatile(register, value) };
    // Preserve the PLIC output before later device/CSR outputs, including
    // interrupt-enable changes, without unnecessarily constraining reads.
    B::data_memory(BarrierDomain::FullSystem, BarrierAccess::Writes);
}
