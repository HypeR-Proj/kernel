// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;

use hyper::hal::interrupt::{
    InterruptController, InterruptId, InterruptPriority, InterruptTransitionError,
    InterruptTrigger, KernelInterruptController, LocalInterruptController,
};
use hyper_platform::InterruptControllerInfo;

const IA32_APIC_BASE: u32 = 0x1b;
const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_BASE_X2APIC: u64 = 1 << 10;
const X2APIC_EOI: u32 = 0x80b;
const X2APIC_ICR: u32 = 0x830;
const X2APIC_SIVR: u32 = 0x80f;
const X2APIC_LVT_TIMER: u32 = 0x832;
const X2APIC_TIMER_INITIAL_COUNT: u32 = 0x838;
const X2APIC_TIMER_CURRENT_COUNT: u32 = 0x839;
const X2APIC_TIMER_DIVIDE: u32 = 0x83e;
const APIC_SOFTWARE_ENABLE: u64 = 1 << 8;
const APIC_TIMER_MASK: u64 = 1 << 16;
const APIC_TIMER_TSC_DEADLINE: u64 = 2 << 17;
const SPURIOUS_VECTOR: u64 = 0xff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Unsupported,
    InvalidCpu,
    InvalidInterrupt,
}

pub struct X2ApicController;
pub struct X2ApicLocalController;

impl LocalInterruptController for X2ApicLocalController {
    type Error = Error;

    fn configure(
        &self,
        interrupt: InterruptId,
        _priority: InterruptPriority,
        trigger: InterruptTrigger,
    ) -> Result<(), Error> {
        if matches!(
            interrupt.get(),
            crate::arch::x86_64::platform::TIMER_VECTOR
                | crate::arch::x86_64::platform::RESCHEDULE_VECTOR
                | crate::arch::x86_64::platform::KERNEL_RPC_VECTOR
        ) && trigger == InterruptTrigger::Edge
        {
            Ok(())
        } else {
            Err(Error::InvalidInterrupt)
        }
    }

    fn enable(&self, interrupt: InterruptId) -> Result<(), InterruptTransitionError<Error>> {
        if interrupt.get() == crate::arch::x86_64::platform::KERNEL_RPC_VECTOR {
            return Ok(());
        }
        let mut controller = X2ApicController;
        controller.enable(interrupt)
    }

    fn disable(&self, interrupt: InterruptId) -> Result<(), InterruptTransitionError<Error>> {
        if interrupt.get() == crate::arch::x86_64::platform::KERNEL_RPC_VECTOR {
            return Ok(());
        }
        let mut controller = X2ApicController;
        controller.disable(interrupt)
    }
}

impl X2ApicController {
    pub unsafe fn bind(
        info: InterruptControllerInfo,
        _map: impl FnMut(u64) -> Option<usize>,
    ) -> Result<Self, Error> {
        if !matches!(info, InterruptControllerInfo::X2Apic(_)) {
            return Err(Error::Unsupported);
        }
        mask_legacy_pic();
        let mut base = read_msr(IA32_APIC_BASE);
        base |= APIC_BASE_ENABLE | APIC_BASE_X2APIC;
        write_msr(IA32_APIC_BASE, base);
        write_msr(X2APIC_SIVR, APIC_SOFTWARE_ENABLE | SPURIOUS_VECTOR);
        Ok(Self)
    }
}

fn mask_legacy_pic() {
    // SAFETY: These are the architectural PIC mask ports and OUT is valid at CPL0.
    unsafe {
        asm!("out dx, al", in("dx") 0x21_u16, in("al") 0xff_u8, options(nostack));
        asm!("out dx, al", in("dx") 0xa1_u16, in("al") 0xff_u8, options(nostack));
    }
}

impl InterruptController for X2ApicController {
    type Error = Error;

    fn enable(
        &mut self,
        interrupt: InterruptId,
    ) -> Result<(), InterruptTransitionError<Self::Error>> {
        if interrupt.get() == crate::arch::x86_64::platform::TIMER_VECTOR {
            write_msr(
                X2APIC_LVT_TIMER,
                APIC_TIMER_TSC_DEADLINE | u64::from(interrupt.get()),
            );
            return Ok(());
        }
        if interrupt.get() == crate::arch::x86_64::platform::RESCHEDULE_VECTOR {
            return crate::arch::x86_64::smp::set_reschedule_enabled(true)
                .then_some(())
                .ok_or(InterruptTransitionError::NotApplied(Error::InvalidCpu));
        }
        Err(InterruptTransitionError::NotApplied(
            Error::InvalidInterrupt,
        ))
    }

    fn disable(
        &mut self,
        interrupt: InterruptId,
    ) -> Result<(), InterruptTransitionError<Self::Error>> {
        if interrupt.get() == crate::arch::x86_64::platform::TIMER_VECTOR {
            mask_timer();
            return Ok(());
        }
        if interrupt.get() == crate::arch::x86_64::platform::RESCHEDULE_VECTOR {
            return crate::arch::x86_64::smp::set_reschedule_enabled(false)
                .then_some(())
                .ok_or(InterruptTransitionError::NotApplied(Error::InvalidCpu));
        }
        Err(InterruptTransitionError::NotApplied(
            Error::InvalidInterrupt,
        ))
    }

    fn acknowledge(&self) -> Option<InterruptId> {
        None
    }

    fn end(&self, _interrupt: InterruptId) {
        write_msr(X2APIC_EOI, 0);
    }
}

impl KernelInterruptController for X2ApicController {
    type Local = X2ApicLocalController;

    fn interrupt_count(&self) -> u32 {
        256
    }

    fn configure(
        &mut self,
        interrupt: InterruptId,
        _priority: InterruptPriority,
        trigger: InterruptTrigger,
    ) -> Result<(), Self::Error> {
        if matches!(
            interrupt.get(),
            crate::arch::x86_64::platform::TIMER_VECTOR
                | crate::arch::x86_64::platform::RESCHEDULE_VECTOR
        ) && trigger == InterruptTrigger::Edge
        {
            Ok(())
        } else {
            Err(Error::InvalidInterrupt)
        }
    }

    fn is_per_cpu(&self, interrupt: InterruptId) -> bool {
        matches!(
            interrupt.get(),
            crate::arch::x86_64::platform::TIMER_VECTOR
                | crate::arch::x86_64::platform::RESCHEDULE_VECTOR
                | crate::arch::x86_64::platform::KERNEL_RPC_VECTOR
        )
    }

    fn local_controller(&self) -> Result<Self::Local, Self::Error> {
        Ok(X2ApicLocalController)
    }

    unsafe fn initialize_local(&mut self) -> Result<Self::Local, Self::Error> {
        let mut base = read_msr(IA32_APIC_BASE);
        base |= APIC_BASE_ENABLE | APIC_BASE_X2APIC;
        write_msr(IA32_APIC_BASE, base);
        write_msr(X2APIC_SIVR, APIC_SOFTWARE_ENABLE | SPURIOUS_VECTOR);
        Ok(X2ApicLocalController)
    }
}

pub fn mask_timer() {
    write_msr(
        X2APIC_LVT_TIMER,
        APIC_TIMER_MASK
            | APIC_TIMER_TSC_DEADLINE
            | u64::from(crate::arch::x86_64::platform::TIMER_VECTOR),
    );
}

pub fn unmask_timer() {
    write_msr(
        X2APIC_LVT_TIMER,
        APIC_TIMER_TSC_DEADLINE | u64::from(crate::arch::x86_64::platform::TIMER_VECTOR),
    );
}

pub fn program_oneshot_timer(count: u32) {
    write_msr(X2APIC_TIMER_DIVIDE, 3);
    write_msr(
        X2APIC_LVT_TIMER,
        u64::from(crate::arch::x86_64::platform::TIMER_VECTOR),
    );
    write_msr(X2APIC_TIMER_INITIAL_COUNT, u64::from(count));
}

pub fn timer_current_count() -> u32 {
    read_msr(X2APIC_TIMER_CURRENT_COUNT) as u32
}

pub fn begin_timer_calibration() {
    write_msr(X2APIC_TIMER_DIVIDE, 3);
    write_msr(
        X2APIC_LVT_TIMER,
        APIC_TIMER_MASK | u64::from(crate::arch::x86_64::platform::TIMER_VECTOR),
    );
    write_msr(X2APIC_TIMER_INITIAL_COUNT, u64::from(u32::MAX));
}

pub fn end_timer_calibration() -> u32 {
    let elapsed = u32::MAX.wrapping_sub(timer_current_count());
    write_msr(X2APIC_TIMER_INITIAL_COUNT, 0);
    elapsed
}

/// Sends one fixed-delivery interrupt to a physical x2APIC destination.
///
/// The x2APIC APIC-register form of WRMSR may complete before older stores
/// become globally visible. Intel SDM Vol. 3A, x2APIC MSR Access, prescribes
/// MFENCE;LFENCE as the non-serializing-instruction alternative; that exact
/// sequence orders a published kernel condition before the target observes
/// this one-shot prompt. No fence is needed after the ICR write.
pub fn send_fixed_ipi(target: u32, interrupt: InterruptId) -> bool {
    let vector = interrupt.get();
    if !(32..SPURIOUS_VECTOR as u32).contains(&vector) {
        return false;
    }
    let value = (u64::from(target) << 32) | u64::from(vector);
    // SAFETY: Controller binding selected x2APIC mode before any reschedule
    // route is enabled. The destination is a published physical x2APIC ID and
    // the vector excludes architectural exceptions and the spurious vector.
    unsafe {
        asm!(
            "mfence",
            "lfence",
            "wrmsr",
            in("ecx") X2APIC_ICR,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack),
        )
    };
    true
}

/// Completes one architecture-private local interrupt.
pub(crate) fn end_local_interrupt() {
    write_msr(X2APIC_EOI, 0);
}

fn read_msr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: Callers use APIC MSRs validated during controller binding.
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack),
        )
    };
    (u64::from(high) << 32) | u64::from(low)
}

fn write_msr(msr: u32, value: u64) {
    // SAFETY: Callers use APIC MSRs validated during controller binding.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack),
        )
    };
}
