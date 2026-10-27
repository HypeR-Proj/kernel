// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime-mediated power requests, published only after vCPU hardware detach.

use super::{Error, InstalledMachine, RuntimeState};
use crate::object::SignalMask;
use {
    hyper_vm_models::arm::psci::Continuation, hyper_vm_models::arm::psci::Operation,
    hyper_vm_models::arm::psci::Request,
};

impl InstalledMachine {
    // Used by guest firmware adapters on platforms with CPU power calls.
    #[allow(
        dead_code,
        reason = "guest PSCI power requests are currently AArch64-only"
    )]
    pub(crate) fn stage_power(
        &self,
        source: u32,
        operation: Operation,
        arguments: [u64; 3],
    ) -> Result<(), i64> {
        let config = self.configuration;
        self.power.with(|power| {
            power.stage(
                source,
                operation,
                arguments,
                config.guest_physical_base..config.guest_physical_base + config.memory_size,
            )
        })
    }

    #[allow(
        dead_code,
        reason = "guest PSCI power requests are currently AArch64-only"
    )]
    pub(crate) fn affinity(&self, target: u64, level: u64) -> i64 {
        self.power.with(|power| power.affinity(target, level))
    }

    pub(crate) fn publish_power(&self, source: u32) -> bool {
        self.power.with(|power| {
            let changed = power.publish(source).unwrap_or_else(|_| {
                crate::diagnostics::crash::fatal(format_args!(
                    "invalid vCPU power publication source {source}"
                ))
            });
            if changed {
                self.update_power_signal(power);
            }
            changed
        })
    }

    pub(crate) fn power_continuation(&self, source: u32) -> Continuation {
        self.power
            .with(|power| power.continuation(source))
            .unwrap_or_else(|_| {
                crate::diagnostics::crash::fatal(format_args!(
                    "invalid vCPU power continuation source {source}"
                ))
            })
    }

    pub(crate) fn pending_power_request(&self) -> Option<Request> {
        self.power.with(|power| power.pending())
    }

    pub(in crate::vm) fn complete_power_request(&self, id: u64, accept: bool) -> Result<(), Error> {
        // Stop publication and power-on readiness have one lifecycle lock.
        // All configured endpoint Threads already exist; this commit allocates
        // nothing and cannot race VM retirement into starting a new Thread.
        let vm_id = self.state.with(|state| {
            let vm_id = match state {
                RuntimeState::Running { id, .. } => *id,
                _ => return Err(Error::BadState),
            };
            let request = self.power.with(|power| {
                let request = power.request(id).ok_or(Error::BadState)?;
                if accept && request.operation == Operation::CpuOff {
                    let vm_id = match state {
                        RuntimeState::Running { id, .. } => *id,
                        _ => return Err(Error::BadState),
                    };
                    crate::vm::registry::with_binding(vm_id, |binding| {
                        crate::hal::vm::reset_vcpu_interrupts(binding.interrupts(), request.vcpu)
                    })
                    .map_err(|_| Error::BadState)?
                    .map_err(|_| Error::BadState)?;
                }
                let request = power.complete(id, accept).map_err(|_| Error::BadState)?;
                self.update_power_signal(power);
                Ok::<_, Error>(request)
            })?;
            if accept && request.operation == Operation::CpuOn {
                let endpoint = self.endpoint(request.target)?;
                match endpoint.lifecycle().map_err(|_| Error::BadState)? {
                    crate::vm::endpoint_state::Lifecycle::Dormant => {
                        // Power state and bootstrap are committed above. This
                        // stable dormant scheduler owner must become runnable.
                        if endpoint.start().is_err() {
                            hyper::debug::invariant_failure(
                                "vm::power::complete_power_request invariant",
                            );
                        }
                    }
                    crate::vm::endpoint_state::Lifecycle::Started => endpoint.signal_waiter(),
                    _ => hyper::debug::invariant_failure(
                        "vm::power::complete_power_request invariant",
                    ),
                }
            }
            self.endpoint(request.vcpu)?.signal_waiter();
            Ok(vm_id)
        })?;
        // Reset may release an active SPI to another vCPU. Publish its prompt
        // only after lifecycle/power locks are released. Concurrent retirement
        // may already have removed the VM; then there is no guest to notify.
        let _ = crate::vm::registry::with_binding(vm_id, |binding| {
            binding.publish_changed_interrupts();
        });
        Ok(())
    }

    fn update_power_signal(&self, power: &hyper_vm_models::arm::psci::PowerState) {
        let bit = SignalMask::from_trusted_bits(
            hyper::abi::native::HYPER_NATIVE_SIGNAL_VIRTUAL_MACHINE_POWER_REQUEST,
        );
        let (clear, set) = if power.pending().is_some() {
            (SignalMask::EMPTY, bit)
        } else {
            (bit, SignalMask::EMPTY)
        };
        if self.vm_signals.update(clear, set).is_err() {
            hyper::debug::invariant_failure("vm::power::update_power_signal invariant");
        }
    }

    pub(crate) fn publish_vcpu_terminal(&self) {
        let bit = SignalMask::from_trusted_bits(
            hyper::abi::native::HYPER_NATIVE_SIGNAL_VIRTUAL_MACHINE_VCPU_TERMINATED,
        );
        if self.vm_signals.update(SignalMask::EMPTY, bit).is_err() {
            hyper::debug::invariant_failure("vm::power::publish_vcpu_terminal invariant");
        }
    }
}
