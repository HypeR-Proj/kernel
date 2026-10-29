// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded scheduling records; permission checking occurs atomically with the
//! target's old configuration in the scheduler or process-builder transaction.

use crate::abi::native::wire::{
    copy_extensible_input_record, copy_info_record, prepare_info_request, require_zero,
};
use hyper::abi::native::{self as abi, NativeResult};
use {crate::abi::native::Arguments, crate::abi::native::TaskServices};
use {
    crate::abi::native::status::status_from_object_service_error,
    crate::abi::native::status::status_only,
};
use {crate::capability::InvocationReceiver, crate::capability::Rights};
use {
    crate::task::policy::BudgetParameters, crate::task::policy::FairBackend,
    crate::task::policy::SchedulingParameters, crate::task::policy::SchedulingPolicy,
    crate::task::policy::SchedulingRole, crate::task::policy::ThreadPriority,
    crate::task::policy::ThreadWeight,
};

#[inline(never)]
pub(in crate::abi::native) fn sys_get_scheduling(
    services: &impl TaskServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeResult {
    crate::abi::native::status::info_result((|| {
        let output = prepare_info_request(arguments, 64, 64)?;
        let params = services
            .get_scheduling(receiver)
            .map_err(status_from_object_service_error)?;
        copy_info_record(services, output, &encode(params))
    })())
}

#[inline(never)]
pub(in crate::abi::native) fn sys_set_scheduling(
    services: &impl TaskServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
    builder: bool,
) -> NativeResult {
    status_only((|| {
        require_zero(&arguments[if builder { 3 } else { 2 }..])?;
        let bytes = copy_extensible_input_record::<64>(services, arguments[0], arguments[1], 64)?;
        let params = decode(&bytes).ok_or(abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        let delegated = if builder {
            Some(Rights::from_bits(arguments[2]).ok_or(abi::HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?)
        } else {
            None
        };
        services
            .set_scheduling(receiver, params, delegated)
            .map_err(status_from_object_service_error)
    })())
}

fn u32_at(bytes: &[u8; 64], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
fn u64_at(bytes: &[u8; 64], at: usize) -> u64 {
    u64::from_le_bytes([
        bytes[at],
        bytes[at + 1],
        bytes[at + 2],
        bytes[at + 3],
        bytes[at + 4],
        bytes[at + 5],
        bytes[at + 6],
        bytes[at + 7],
    ])
}

fn decode(bytes: &[u8; 64]) -> Option<SchedulingParameters> {
    let band = ThreadPriority::new(u8::try_from(u32_at(bytes, 4)).ok()?);
    let role = match u32_at(bytes, 8) {
        0 => SchedulingRole::Ordinary,
        1 => SchedulingRole::Priority,
        2 => SchedulingRole::Protected,
        _ => return None,
    };
    if u64_at(bytes, 56) != 0 {
        return None;
    }
    match u32_at(bytes, 0) {
        1 => {
            if u32_at(bytes, 16) != 0
                || u64_at(bytes, 32) != 0
                || u64_at(bytes, 40) != 0
                || u64_at(bytes, 48) != 0
            {
                return None;
            }
            let backend = match u32_at(bytes, 20) {
                0 => FairBackend::Stride,
                1 => FairBackend::RoundRobin,
                2 => crate::task::scheduler::default_parameters(SchedulingPolicy::Fair)
                    .fair_backend(),
                _ => return None,
            };
            let weight = ThreadWeight::new(u16::try_from(u32_at(bytes, 12)).ok()?)?;
            SchedulingParameters::fair(weight, u64_at(bytes, 24), band, role, backend)
        }
        2 => {
            if u32_at(bytes, 12) != 0
                || u32_at(bytes, 16) != 0
                || u32_at(bytes, 20) != 0
                || u64_at(bytes, 24) != 0
            {
                return None;
            }
            let budget =
                BudgetParameters::new(u64_at(bytes, 32), u64_at(bytes, 40), u64_at(bytes, 48))?;
            Some(SchedulingParameters::budget(budget, band, role))
        }
        3 => {
            if u32_at(bytes, 12) != 0
                || u32_at(bytes, 20) != 0
                || u64_at(bytes, 24) != 0
                || u64_at(bytes, 32) != 0
                || u64_at(bytes, 40) != 0
                || u64_at(bytes, 48) != 0
            {
                return None;
            }
            Some(SchedulingParameters::fifo(
                ThreadPriority::new(u8::try_from(u32_at(bytes, 16)).ok()?),
                band,
                role,
            ))
        }
        _ => None,
    }
}

fn encode(params: SchedulingParameters) -> [u8; 64] {
    let mut bytes = [0; 64];
    let policy: u32 = match params.policy() {
        SchedulingPolicy::Fair => 1,
        SchedulingPolicy::Budget => 2,
        SchedulingPolicy::Fifo { .. } => 3,
        SchedulingPolicy::Idle => 0,
    };
    bytes[0..4].copy_from_slice(&policy.to_le_bytes());
    bytes[4..8].copy_from_slice(&u32::from(params.band().get()).to_le_bytes());
    let role: u32 = match params.role() {
        SchedulingRole::Ordinary => 0,
        SchedulingRole::Priority => 1,
        SchedulingRole::Protected => 2,
    };
    bytes[8..12].copy_from_slice(&role.to_le_bytes());
    match params.policy() {
        SchedulingPolicy::Fair => {
            bytes[12..16].copy_from_slice(&u32::from(params.weight().get()).to_le_bytes());
            let backend: u32 = match params.fair_backend() {
                FairBackend::Stride => 0,
                FairBackend::RoundRobin => 1,
            };
            bytes[20..24].copy_from_slice(&backend.to_le_bytes());
            bytes[24..32].copy_from_slice(&params.quantum_ns().to_le_bytes());
        }
        SchedulingPolicy::Fifo { priority } => {
            bytes[16..20].copy_from_slice(&u32::from(priority.get()).to_le_bytes())
        }
        SchedulingPolicy::Budget => {
            if let Some(budget) = params.budget_parameters() {
                bytes[32..40].copy_from_slice(&budget.budget_ns().to_le_bytes());
                bytes[40..48].copy_from_slice(&budget.period_ns().to_le_bytes());
                bytes[48..56].copy_from_slice(&budget.burst_ns().to_le_bytes());
            }
        }
        SchedulingPolicy::Idle => {}
    }
    bytes
}
