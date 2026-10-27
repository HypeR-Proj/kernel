// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected typed VM-exit service table.
//!
//! Target-specific copied event values stay in this narrow boundary. Common
//! memory-fault policy remains in the parent module.

pub(super) const fn services() -> crate::hal::vm::ExitServices {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        crate::hal::vm::ExitServices::aarch64(
            super::dispatch_memory_fault,
            dispatch_mmio,
            dispatch_guest_sync,
        )
    }
    #[cfg(CONFIG_ARCH_RISCV64)]
    {
        crate::hal::vm::ExitServices::riscv64(
            super::dispatch_memory_fault,
            dispatch_mmio,
            dispatch_guest_sync,
        )
    }
    #[cfg(CONFIG_ARCH_X86_64)]
    {
        crate::hal::vm::ExitServices::x86_64(
            super::dispatch_memory_fault,
            dispatch_port_io,
            query_pending_interrupt,
        )
    }
}

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
fn dispatch_mmio(access: hyper_vm_models::exit::MmioAccess) -> hyper_vm_models::exit::MmioAction {
    match crate::vm::active_vcpu::with(|execution| {
        crate::vm::device::selected::dispatch_mmio(execution, access)
    }) {
        Ok(Some(dispatch)) => dispatch.into_action(),
        Ok(None) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: guest MMIO exit arrived without an active vCPU"
        )),
        Err(error) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: invalid guest MMIO entry context: {error:?}"
        )),
    }
}

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
fn dispatch_guest_sync(exit: crate::hal::vm::GuestSyncExit) -> crate::hal::vm::GuestSyncAction {
    match crate::vm::active_vcpu::with(|execution| {
        #[cfg(CONFIG_ARCH_RISCV64)]
        if let Some(byte) = exit.legacy_console_byte() {
            if !crate::vm::device::selected::write_console_byte(execution, byte) {
                return crate::hal::vm::GuestSyncAction::Stop;
            }
            return crate::hal::vm::GuestSyncAction::complete_legacy_console();
        }
        #[cfg(CONFIG_ARCH_AARCH64)]
        if let Some(action) = dispatch_power_call(execution, exit) {
            return action;
        }
        let (hardware, vcpu_id, interrupts) = execution.interrupt_context();
        let action = crate::hal::vm::handle_guest_sync(hardware, vcpu_id, interrupts, exit);
        #[cfg(CONFIG_ARCH_AARCH64)]
        if let Some(binding) = execution.vm_binding() {
            binding.publish_changed_interrupts();
        }
        action
    }) {
        Ok(Some(action)) => action,
        Ok(None) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: synchronous guest exit arrived without an active vCPU"
        )),
        Err(error) => crate::diagnostics::crash::fatal(format_args!(
            "HypeR: invalid synchronous guest entry context: {error:?}"
        )),
    }
}

#[cfg(CONFIG_ARCH_X86_64)]
fn dispatch_port_io(
    exit: hyper_vm_models::x86::exit::PortIoExit,
) -> hyper_vm_models::x86::exit::PortIoAction {
    use {hyper_vm_models::x86::exit::PortIoAction, hyper_vm_models::x86::exit::PortIoOperation};

    match crate::vm::device::selected::access_port(exit) {
        Ok(value) => match (exit.operation(), value) {
            (PortIoOperation::Input, Some(value)) => PortIoAction::CompleteInput(value),
            (PortIoOperation::Output(_), _) => PortIoAction::CompleteOutput,
            (PortIoOperation::Input, None) => PortIoAction::Stop,
        },
        Err(error) => {
            crate::pr_err!("HypeR: x86 guest port-I/O dispatch failed: {error:?}");
            PortIoAction::Stop
        }
    }
}

#[cfg(CONFIG_ARCH_X86_64)]
fn query_pending_interrupt(
    timer_pending: bool,
) -> hyper_vm_models::x86::exit::PendingInterruptAction {
    use hyper_vm_models::x86::device::legacy_pc::InterruptSource;
    use hyper_vm_models::x86::exit::PendingInterruptAction;

    match crate::vm::device::selected::pending_interrupt(timer_pending) {
        Ok(Some(pending)) => PendingInterruptAction::Inject {
            vector: pending.vector,
            consumes_timer: pending.source == InterruptSource::Timer,
        },
        Ok(None) => PendingInterruptAction::None,
        Err(error) => {
            crate::pr_err!("HypeR: x86 guest interrupt routing failed: {error:?}");
            PendingInterruptAction::Stop
        }
    }
}

#[cfg(CONFIG_ARCH_AARCH64)]
fn dispatch_power_call(
    execution: &mut crate::vm::vcpu::VcpuExecution,
    exit: crate::hal::vm::GuestSyncExit,
) -> Option<crate::hal::vm::GuestSyncAction> {
    let crate::hal::vm::GuestSyncExit::HypervisorCall {
        function,
        argument,
        extra,
    } = exit
    else {
        return None;
    };
    let binding = execution.vm_binding()?;
    let owner = binding.lifecycle();
    let mut arguments = [argument, extra[0], extra[1]];
    if function & (1 << 30) == 0 {
        arguments = arguments.map(|value| value as u32 as u64);
    }
    let reply = |value: i64| crate::hal::vm::GuestSyncAction::WriteRegister {
        register: 0,
        value: hyper_vm_models::arm::psci::return_register(value),
        advance: false,
    };
    if matches!(function, 0x8400_0004 | 0xc400_0004) {
        return Some(reply(owner.affinity(arguments[0], arguments[1])));
    }
    let operation = match function {
        0x8400_0002 => hyper_vm_models::arm::psci::Operation::CpuOff,
        0x8400_0003 | 0xc400_0003 => hyper_vm_models::arm::psci::Operation::CpuOn,
        0x8400_0008 => hyper_vm_models::arm::psci::Operation::SystemOff,
        0x8400_0009 => hyper_vm_models::arm::psci::Operation::SystemReset,
        _ => return None,
    };
    Some(
        match owner.stage_power(execution.vcpu_id, operation, arguments) {
            Ok(()) => crate::hal::vm::GuestSyncAction::FirmwareWait,
            Err(error) => reply(error),
        },
    )
}
