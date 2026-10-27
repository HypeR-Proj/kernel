// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Per-VM interrupt topology for x86 hardware virtualization.

use hyper_vm_models::interrupt::VirtualInterruptId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidVcpuCount,
}

pub struct VmInterruptController;

impl VmInterruptController {
    pub fn new(vcpu_count: u32, _timer_interrupt: VirtualInterruptId) -> Result<Self, Error> {
        if vcpu_count == 0 {
            return Err(Error::InvalidVcpuCount);
        }
        Ok(Self)
    }
}
