// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::platform::{
    CpuPowerInfo, InterruptControllerInfo, PlatformInterrupt, PlatformInterruptTrigger, TimerInfo,
    TimerKind, X2ApicInfo, X86ApicInfo,
    fdt::{NodeId, NodeVisitor, Property},
};

pub const TIMER_VECTOR: u32 = 0xef;
pub const RESCHEDULE_VECTOR: u32 = 0xf0;
pub const KERNEL_RPC_VECTOR: u32 = 0xf1;

const _: () = assert!(KERNEL_RPC_VECTOR >= 32);
const _: () = assert!(KERNEL_RPC_VECTOR < 0xff);
const _: () = assert!(KERNEL_RPC_VECTOR != TIMER_VECTOR);
const _: () = assert!(KERNEL_RPC_VECTOR != RESCHEDULE_VECTOR);

#[derive(Clone, Copy, Debug)]
pub struct EssentialPlatformInfo {
    pub cpu_power: Option<CpuPowerInfo>,
    pub interrupt_controller: Option<InterruptControllerInfo>,
    pub timer: Option<TimerInfo>,
    pub tsc_frequency: u64,
    claims: [Option<hyper::platform::fdt::NodeId>; 0],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidInterrupt,
    InvalidProperty,
    MissingInvariantTsc,
    MissingTscFrequency,
    MissingX2Apic,
}

pub struct EssentialDeviceDiscovery {
    tsc_frequency: Option<u64>,
}

impl EssentialDeviceDiscovery {
    pub const fn new() -> Self {
        Self {
            tsc_frequency: None,
        }
    }

    pub fn finish(self) -> Result<EssentialPlatformInfo, Error> {
        let basic = core::arch::x86_64::__cpuid(1);
        if basic.ecx & (1 << 21) == 0 {
            return Err(Error::MissingX2Apic);
        }
        let extended_max = core::arch::x86_64::__cpuid(0x8000_0000).eax;
        let invariant_tsc = extended_max >= 0x8000_0007
            && core::arch::x86_64::__cpuid(0x8000_0007).edx & (1 << 8) != 0;
        if !invariant_tsc && !super::features::running_under_qemu_tcg() {
            return Err(Error::MissingInvariantTsc);
        }
        let frequency = self
            .tsc_frequency
            .or_else(super::features::tsc_frequency)
            .ok_or(Error::MissingTscFrequency)?;
        Ok(EssentialPlatformInfo {
            cpu_power: Some(CpuPowerInfo::X86Apic(X86ApicInfo {
                tsc_frequency: frequency,
            })),
            interrupt_controller: Some(InterruptControllerInfo::X2Apic(X2ApicInfo)),
            timer: Some(TimerInfo {
                kind: TimerKind::X86TscDeadline,
                virtual_timer: PlatformInterrupt {
                    interrupt: TIMER_VECTOR,
                    trigger: PlatformInterruptTrigger::Edge,
                },
                hypervisor_physical: PlatformInterrupt {
                    interrupt: TIMER_VECTOR,
                    trigger: PlatformInterruptTrigger::Edge,
                },
            }),
            tsc_frequency: frequency,
            claims: [],
        })
    }
}

impl EssentialPlatformInfo {
    pub fn claims(&self) -> &[Option<hyper::platform::fdt::NodeId>] {
        &self.claims
    }
}

impl Default for EssentialDeviceDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeVisitor for EssentialDeviceDiscovery {
    type Error = Error;

    fn property(&mut self, _id: NodeId, property: Property<'_>) -> Result<(), Self::Error> {
        if property.name() == "hyper,tsc-frequency-hz" {
            self.tsc_frequency = Some(property.u64().map_err(|_| Error::InvalidProperty)?);
        }
        Ok(())
    }
}

pub fn decode_platform_interrupt(descriptor: &[u32]) -> Result<PlatformInterrupt, Error> {
    let interrupt = *descriptor.first().ok_or(Error::InvalidInterrupt)?;
    Ok(PlatformInterrupt {
        interrupt,
        trigger: PlatformInterruptTrigger::Edge,
    })
}
