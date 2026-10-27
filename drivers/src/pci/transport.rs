// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exclusive physical PCI functions with host-owned configuration and IRQ routing.

use super::{Bar, BcmTransport, Error, FunctionState, Prepared, ecam};
use hyper::hal::barrier::Barrier;
use {crate::platform::DriverServices, crate::platform::PlatformDevice};

pub struct Transport(Backend);
enum Backend {
    Bcm(BcmTransport),
    Ecam(ecam::Function),
}

impl Transport {
    /// Only the standard virtio reset acknowledgement proves DMA quiescence
    /// for ECAM devices. BCM2712's bus-master gate alone is not such a proof.
    pub fn reset(&self) -> bool {
        match &self.0 {
            Backend::Bcm(_) => false,
            Backend::Ecam(function) => function.reset(),
        }
    }
    pub(super) fn bcm(transport: BcmTransport) -> Self {
        Self(Backend::Bcm(transport))
    }
    pub(super) fn ecam(transport: ecam::Function) -> Self {
        Self(Backend::Ecam(transport))
    }
    pub fn discover<B: Barrier>(
        nodes: &[PlatformDevice],
        services: &dyn DriverServices,
    ) -> Result<Option<Prepared>, Error> {
        BcmTransport::discover::<B>(nodes, services)
    }
    pub fn discover_ecam<B: Barrier>(
        nodes: &[PlatformDevice],
        services: &dyn DriverServices,
    ) -> Result<alloc::vec::Vec<Prepared>, Error> {
        ecam::discover::<B>(nodes, services)
    }
    pub fn stream(&self) -> Option<u32> {
        match &self.0 {
            Backend::Bcm(_) => None,
            Backend::Ecam(t) => Some(t.stream),
        }
    }
    pub fn uses_intx(&self) -> bool {
        matches!(self.0, Backend::Ecam(_))
    }
    pub fn line_asserted(&self) -> bool {
        match &self.0 {
            Backend::Bcm(_) => false,
            Backend::Ecam(t) => t.line_asserted(),
        }
    }
    pub fn identity(&self) -> u32 {
        match &self.0 {
            Backend::Bcm(t) => t.identity(),
            Backend::Ecam(t) => t.identity(),
        }
    }
    pub fn dma_offset(&self) -> u64 {
        match &self.0 {
            Backend::Bcm(t) => t.dma_offset(),
            Backend::Ecam(_) => 0,
        }
    }
    pub fn bars(&self) -> &[Option<Bar>; 6] {
        match &self.0 {
            Backend::Bcm(t) => t.bars(),
            Backend::Ecam(t) => &t.bars,
        }
    }
    pub fn state(&self, base: u64, irq: u32) -> FunctionState {
        match &self.0 {
            Backend::Bcm(t) => t.state(base, irq),
            Backend::Ecam(t) => t.state(base, irq),
        }
    }
    pub fn activate(&self, state: &mut FunctionState) {
        match &self.0 {
            Backend::Bcm(t) => t.activate(state),
            Backend::Ecam(t) => t.activate(state),
        }
    }
    pub fn stop(&self, state: &mut FunctionState) {
        match &self.0 {
            Backend::Bcm(t) => t.stop(state),
            Backend::Ecam(t) => t.stop(state),
        }
    }
    pub fn read(&self, state: &mut FunctionState, offset: usize, width: usize) -> Option<u64> {
        match &self.0 {
            Backend::Bcm(t) => t.read(state, offset, width),
            Backend::Ecam(t) => t.read(state, offset, width),
        }
    }
    pub fn write(
        &self,
        state: &mut FunctionState,
        offset: usize,
        width: usize,
        value: u64,
    ) -> bool {
        match &self.0 {
            Backend::Bcm(t) => t.write(state, offset, width, value),
            Backend::Ecam(t) => t.write(state, offset, width, value),
        }
    }
}
