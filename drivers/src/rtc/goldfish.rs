// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Read-only Goldfish RTC sampling; no alarm, IRQ, or counter programming.
//! Register protocol: Android GOLDFISH-VIRTUAL-HARDWARE.TXT, "Goldfish Timer".

use core::marker::PhantomData;

use hyper::hw::goldfish_rtc as reg;
use {crate::platform::MmioMappingError, crate::platform::PermanentMmioMapping};
use {
    hyper::hal::barrier::Barrier, hyper::hal::barrier::BarrierAccess,
    hyper::hal::barrier::BarrierDomain,
};

pub struct Goldfish<B: Barrier> {
    mapping: PermanentMmioMapping,
    _barrier: PhantomData<B>,
}

impl<B: Barrier> Goldfish<B> {
    pub fn bind(mapping: PermanentMmioMapping) -> Result<Self, MmioMappingError> {
        Ok(Self {
            mapping: mapping.validate_window(reg::REGISTER_WINDOW, 4)?,
            _barrier: PhantomData,
        })
    }

    /// Samples signed nanoseconds since the Unix epoch. The device owner must
    /// serialize all readers of this register window: `TIME_LOW` updates a shared
    /// high-word latch, so simultaneous bindings cannot sample independently.
    pub fn nanoseconds(&mut self) -> i64 {
        let low = self.read(reg::TIME_LOW);
        // Volatile accesses constrain the compiler, not device-read ordering.
        // Complete the low-word latch operation before reading its high word.
        B::data_memory(BarrierDomain::FullSystem, BarrierAccess::Reads);
        let high = self.read(reg::TIME_HIGH);
        B::data_memory(BarrierDomain::FullSystem, BarrierAccess::Reads);
        ((u64::from(high) << 32) | u64::from(low)) as i64
    }

    fn read(&self, offset: usize) -> u32 {
        // SAFETY: bind validated the permanent Device window and alignment;
        // private callers supply only its two aligned read-only register offsets.
        unsafe { core::ptr::read_volatile((self.mapping.virtual_start() + offset) as *const u32) }
    }
}
