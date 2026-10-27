// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Single-vCPU reference-platform PLIC ownership.

use hyper::sync::InterruptSpinLock;
use hyper_vm_models::exit::MmioOperation;
use hyper_vm_models::interrupt::VirtualInterruptId;
use hyper_vm_models::riscv64::plic::VirtualPlic;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidVcpuCount,
    InvalidAccess,
    Model(hyper_vm_models::riscv64::plic::Error),
}

pub struct VmInterruptController {
    plic: InterruptSpinLock<VirtualPlic, crate::arch::riscv64::LocalInterruptMask>,
}

impl VmInterruptController {
    pub fn new(vcpu_count: u32, _timer_interrupt: VirtualInterruptId) -> Result<Self, Error> {
        if vcpu_count != 1 {
            return Err(Error::InvalidVcpuCount);
        }
        Ok(Self {
            plic: InterruptSpinLock::new(VirtualPlic::new()),
        })
    }

    pub(crate) fn external_pending(&self, vcpu: u32) -> Result<bool, Error> {
        if vcpu != 0 {
            return Err(Error::InvalidVcpuCount);
        }
        Ok(self.plic.with(|plic| plic.interrupt_asserted()))
    }

    pub(crate) fn set_device_level(
        &self,
        vcpu: u32,
        source: u32,
        level: bool,
    ) -> Result<(), Error> {
        if vcpu != 0 {
            return Err(Error::InvalidVcpuCount);
        }
        self.plic
            .with(|plic| plic.set_level(source, level))
            .map_err(Error::Model)
    }

    pub(crate) fn access_plic(
        &self,
        vcpu: u32,
        offset: u64,
        size: usize,
        operation: MmioOperation,
    ) -> Result<Option<u64>, Error> {
        if vcpu != 0 {
            return Err(Error::InvalidVcpuCount);
        }
        if size != 4 || offset & 3 != 0 {
            return Err(Error::InvalidAccess);
        }
        self.plic
            .with(|plic| match operation {
                MmioOperation::Read => plic.read(offset).map(|value| Some(u64::from(value))),
                MmioOperation::Write(value) => plic.write(offset, value as u32).map(|()| None),
            })
            .map_err(Error::Model)
    }
}
