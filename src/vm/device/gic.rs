// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected guest GIC MMIO service for the reference VM board.
//!
//! The reusable device model owns register decoding and state semantics. This
//! service retains only kernel error mapping and selected-controller routing.

use hyper_vm_models::exit::MmioAccess;
use {
    hyper_vm_models::arm::gic::mmio::DecodeError, hyper_vm_models::arm::gic::mmio::DecodedAccess,
    hyper_vm_models::arm::gic::mmio::decode_v3_cpus,
};

use crate::vm::VmInterruptController;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Access(crate::hal::vm::GicAccessError),
    Decode(DecodeError),
}

impl From<DecodeError> for Error {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

pub fn decode(access: MmioAccess, vcpu_count: u32) -> Result<Option<DecodedAccess>, Error> {
    if crate::hal::vm::guest_gic_version() == 2 {
        hyper_vm_models::arm::gic::mmio::decode_v2(access.address(), access.width())
            .map_err(Into::into)
    } else {
        decode_v3_cpus(access.address(), access.width(), vcpu_count).map_err(Into::into)
    }
}

pub fn access(
    hardware: &mut crate::hal::vm::VcpuHardwareState,
    interrupts: &VmInterruptController,
    vcpu_id: u32,
    access: DecodedAccess,
    operation: hyper_vm_models::exit::MmioOperation,
) -> Result<Option<u64>, Error> {
    crate::hal::vm::access_guest_gic(hardware, vcpu_id, interrupts, access, operation)
        .map_err(Error::Access)
}
