// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::hal::interrupt::{
    InterruptController, InterruptId, InterruptPriority, InterruptTransitionError,
    InterruptTrigger, KernelInterruptController, LocalInterruptController,
};
use hyper_platform::InterruptControllerInfo;
use {
    hyper_drivers::interrupt::plic::Error as PlicError, hyper_drivers::interrupt::plic::Plic,
    hyper_drivers::interrupt::plic::PlicLocal,
};

pub struct Riscv64InterruptController(Plic<crate::arch::riscv64::barrier::Riscv64Barrier>);
pub struct Riscv64LocalInterruptController(
    PlicLocal<crate::arch::riscv64::barrier::Riscv64Barrier>,
);

impl LocalInterruptController for Riscv64LocalInterruptController {
    type Error = Error;

    fn configure(
        &self,
        interrupt: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
    ) -> Result<(), Error> {
        self.0
            .configure(interrupt, priority, trigger)
            .map_err(Into::into)
    }

    fn enable(&self, interrupt: InterruptId) -> Result<(), InterruptTransitionError<Error>> {
        self.0
            .enable(interrupt)
            .map_err(|error| error.map(Into::into))
    }

    fn disable(&self, interrupt: InterruptId) -> Result<(), InterruptTransitionError<Error>> {
        self.0
            .disable(interrupt)
            .map_err(|error| error.map(Into::into))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Plic(PlicError),
    Unsupported,
}

impl From<PlicError> for Error {
    fn from(error: PlicError) -> Self {
        Self::Plic(error)
    }
}

impl Riscv64InterruptController {
    /// Binds the PLIC selected by early architecture discovery.
    ///
    /// # Safety
    ///
    /// `map` must return a permanent MMIO mapping owned by the controller.
    pub unsafe fn bind(
        info: InterruptControllerInfo,
        map: impl FnMut(u64) -> Option<usize>,
    ) -> Result<Self, Error> {
        let InterruptControllerInfo::Plic(info) = info else {
            return Err(Error::Unsupported);
        };
        // SAFETY: This function forwards its permanent-mapping contract to PLIC.
        let controller =
            Self(unsafe { Plic::bind(info, map, crate::arch::riscv64::current_hardware_id)? });
        crate::arch::riscv64::irq::enable_external_interrupt_source();
        Ok(controller)
    }
}

impl InterruptController for Riscv64InterruptController {
    type Error = Error;

    fn enable(
        &mut self,
        interrupt: InterruptId,
    ) -> Result<(), InterruptTransitionError<Self::Error>> {
        self.0
            .enable(interrupt)
            .map_err(|error| error.map(Into::into))
    }

    fn disable(
        &mut self,
        interrupt: InterruptId,
    ) -> Result<(), InterruptTransitionError<Self::Error>> {
        self.0
            .disable(interrupt)
            .map_err(|error| error.map(Into::into))
    }

    fn acknowledge(&self) -> Option<InterruptId> {
        self.0.acknowledge()
    }

    fn end(&self, interrupt: InterruptId) {
        self.0.end(interrupt);
    }
}

impl KernelInterruptController for Riscv64InterruptController {
    type Local = Riscv64LocalInterruptController;

    fn interrupt_count(&self) -> u32 {
        self.0.interrupt_count()
    }

    fn configure(
        &mut self,
        interrupt: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
    ) -> Result<(), Self::Error> {
        self.0
            .configure(interrupt, priority, trigger)
            .map_err(Into::into)
    }

    fn is_per_cpu(&self, interrupt: InterruptId) -> bool {
        self.0.is_per_cpu(interrupt)
    }

    fn local_controller(&self) -> Result<Self::Local, Self::Error> {
        self.0
            .local_controller()
            .map(Riscv64LocalInterruptController)
            .map_err(Into::into)
    }

    unsafe fn initialize_local(&mut self) -> Result<Self::Local, Self::Error> {
        // SAFETY: The trait contract guarantees exclusive local-controller setup.
        let local = unsafe { self.0.initialize_local() }
            .map(Riscv64LocalInterruptController)
            .map_err(Error::from)?;
        crate::arch::riscv64::irq::enable_external_interrupt_source();
        Ok(local)
    }
}
