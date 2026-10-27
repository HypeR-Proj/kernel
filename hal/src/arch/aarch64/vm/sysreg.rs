// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `AArch64` guest synchronous-trap and virtual system-register handling.

use core::arch::asm;
use {
    hyper_vm_models::exit::AccessWidth, hyper_vm_models::exit::GuestMemoryFault,
    hyper_vm_models::exit::GuestPhysicalAddress, hyper_vm_models::exit::MemoryAccess,
    hyper_vm_models::exit::MmioAccess, hyper_vm_models::exit::MmioAction,
    hyper_vm_models::exit::MmioOperation,
};

use {crate::arch::aarch64::VcpuContext, crate::arch::aarch64::VmInterruptController};
use {
    crate::arch::aarch64::registers,
    crate::arch::aarch64::registers::SystemRegisterEncoding as Encoding,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum GuestSyncAction {
    /// Consume one trapped `AArch64` instruction.
    Advance,
    /// Write one guest register and optionally consume the instruction.
    WriteRegister {
        register: u8,
        value: u64,
        advance: bool,
    },
    /// Enter the guest's Undefined Instruction vector.
    InjectUndefined {
        program_counter: u64,
        processor_state: u64,
    },
    /// The exit cannot safely return to the guest.
    Stop(GuestSyncFailure),
    /// Complete WFI and return through the typed stopped-vCPU boundary.
    Wait,
    /// HVC already advanced ELR; park until userspace completes its request.
    FirmwareWait,
    /// A GIC transaction must finish after all affected hardware banks detach.
    InterruptAccess,
}

/// Typed failure which prevented a decoded synchronous exit from completing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestSyncFailure {
    VirtualInterrupt(crate::arch::aarch64::vm::vcpu::Error),
}

/// Owned `AArch64` synchronous-exit facts consumed by active-vCPU emulation.
///
/// Register-file references and raw exception frames remain in exception
/// entry. This value is fixed-width and may cross the architecture-to-policy
/// callback without extending a stack-frame borrow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestSyncExit {
    SystemRegister(SystemRegisterExit),
    HypervisorCall {
        function: u64,
        argument: u64,
        extra: [u64; 2],
    },
    SecureMonitorCall,
    Wait(WaitInstruction),
    Undefined(UndefinedExit),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitInstruction {
    Event,
    Interrupt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SystemRegisterExit {
    encoding: Encoding,
    target: u8,
    direction: Direction,
    value: u64,
    undefined: UndefinedExit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UndefinedExit {
    program_counter: u64,
    processor_state: u64,
    syndrome: u64,
}

const _: () = {
    // Entry copies only the operands required by policy. Keep accidental raw
    // register-file snapshots from silently entering this hot-path contract.
    assert!(core::mem::size_of::<GuestSyncExit>() <= 64);
    assert!(core::mem::size_of::<GuestSyncAction>() <= 64);
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationError {
    Completion,
    Decoder,
    Topology,
}

pub fn validate() -> Result<(), ValidationError> {
    let syndrome = (registers::ESR_EC_SYSTEM_REGISTER << registers::ESR_EC_SHIFT)
        | registers::ESR_IL
        | (3 << registers::ESR_SYSREG_OP0_SHIFT)
        | (5 << registers::ESR_SYSREG_OP2_SHIFT)
        | (17 << registers::ESR_SYSREG_RT_SHIFT)
        | registers::ESR_SYSREG_DIRECTION_READ;
    let access = decode_access(syndrome);
    if access.encoding != registers::SYSREG_MPIDR_EL1
        || access.target != 17
        || access.direction != Direction::Read
    {
        return Err(ValidationError::Decoder);
    }
    if virtual_mpidr(0x1234_5678) != 0x0000_0012_0034_5678 {
        return Err(ValidationError::Topology);
    }
    if !validate_guest_memory_fault_decoder() {
        return Err(ValidationError::Decoder);
    }
    if !validate_owned_exit_completion() {
        return Err(ValidationError::Completion);
    }
    if !validate_typed_sync_failure() {
        return Err(ValidationError::Completion);
    }
    Ok(())
}

/// Installs the architected virtual processor identity for one vCPU.
///
/// `MIDR_EL1` and `MPIDR_EL1` are redirected to `VPIDR_EL2` and
/// `VMPIDR_EL2` when their accesses are not trapped. The current guest policy
/// traps them through `HCR_EL2.TID3`, but the virtual registers are still the
/// architectural direct-read fallback and have architecturally unknown reset
/// values. Install a coherent identity on every activation rather than making
/// correctness depend on a particular trap policy.
pub(crate) fn activate_virtual_identity(vcpu_id: u32) {
    let processor = crate::arch::aarch64::vm::cpu_model::processor_identity();
    let affinity = virtual_mpidr(vcpu_id);
    // SAFETY: The caller owns the stopped local guest context at EL2. These
    // registers affect only lower-EL identity reads, and ISB makes both writes
    // effective before guest execution can resume.
    unsafe {
        asm!(
            "msr VPIDR_EL2, {processor}",
            "msr VMPIDR_EL2, {affinity}",
            "isb",
            processor = in(reg) processor,
            affinity = in(reg) affinity,
            options(nostack, preserves_flags)
        );
    }
}

fn validate_typed_sync_failure() -> bool {
    let encoding = registers::SYSREG_ICC_SGI1R_EL1;
    let target = 7u8;
    let syndrome = (registers::ESR_EC_SYSTEM_REGISTER << registers::ESR_EC_SHIFT)
        | (u64::from(encoding.op0) << registers::ESR_SYSREG_OP0_SHIFT)
        | (u64::from(encoding.op1) << registers::ESR_SYSREG_OP1_SHIFT)
        | (u64::from(encoding.crn) << registers::ESR_SYSREG_CRN_SHIFT)
        | (u64::from(encoding.crm) << registers::ESR_SYSREG_CRM_SHIFT)
        | (u64::from(encoding.op2) << registers::ESR_SYSREG_OP2_SHIFT)
        | (u64::from(target) << registers::ESR_SYSREG_RT_SHIFT);
    let request = 0x1234_5678_9abc_def0;
    let mut general = [0u64; 31];
    general[usize::from(target)] = request;
    let Some(exit @ GuestSyncExit::SystemRegister(decoded)) =
        decode_guest_sync(syndrome, &general, 0x8800, registers::SPSR_EL1H_AND_DAIF)
    else {
        return false;
    };
    if decoded.encoding != encoding
        || decoded.target != target
        || decoded.direction != Direction::Write
        || decoded.value != request
    {
        return false;
    }
    let error = crate::arch::aarch64::vm::vcpu::Error::Controller(
        hyper_vm_models::arm::gic::RuntimeError::NotConfigured,
    );
    exit == GuestSyncExit::SystemRegister(decoded)
        && software_interrupt_completion(Err(error))
            == GuestSyncAction::Stop(GuestSyncFailure::VirtualInterrupt(error))
}

fn validate_owned_exit_completion() -> bool {
    let mut general = [0u64; 31];
    general[3] = 0x12cd;
    let write_syndrome = (registers::ESR_EC_DATA_ABORT_LOWER << registers::ESR_EC_SHIFT)
        | registers::ESR_DATA_ABORT_ISV
        | registers::ESR_DATA_ABORT_WNR
        | (3 << registers::ESR_DATA_ABORT_SRT_SHIFT);
    let Some((write, write_completion)) =
        decode_guest_mmio_access(write_syndrome, 0x9000_1000, &general)
    else {
        return false;
    };
    if write.width() != AccessWidth::Byte || write.operation() != MmioOperation::Write(0xcd) {
        return false;
    }
    let mut mismatch_pc = 0x8000;
    if write_completion.apply(&mut general, &mut mismatch_pc, MmioAction::CompleteRead(1))
        || mismatch_pc != 0x8000
    {
        return false;
    }
    let mut write_pc = 0x8000;
    if !write_completion.apply(&mut general, &mut write_pc, MmioAction::CompleteWrite)
        || write_pc != 0x8004
        || general[3] != 0x12cd
    {
        return false;
    }

    let read_syndrome = (registers::ESR_EC_DATA_ABORT_LOWER << registers::ESR_EC_SHIFT)
        | registers::ESR_DATA_ABORT_ISV
        | registers::ESR_DATA_ABORT_SSE
        | (4 << registers::ESR_DATA_ABORT_SRT_SHIFT);
    let Some((read, read_completion)) =
        decode_guest_mmio_access(read_syndrome, 0x9000_2000, &general)
    else {
        return false;
    };
    let mut read_pc = 0x8100;
    if read.operation() != MmioOperation::Read
        || !read_completion.apply(&mut general, &mut read_pc, MmioAction::CompleteRead(0x80))
        || read_pc != 0x8104
        || general[4] != 0xffff_ff80
    {
        return false;
    }

    general[0] = registers::SMCCC_VERSION;
    general[1] = 0xfeed;
    let hvc_syndrome = registers::ESR_EC_HVC64 << registers::ESR_EC_SHIFT;
    let Some(GuestSyncExit::HypervisorCall {
        function,
        argument,
        extra,
    }) = decode_guest_sync(
        hvc_syndrome,
        &general,
        0x8200,
        registers::SPSR_EL1H_AND_DAIF,
    )
    else {
        return false;
    };
    let hvc_exit = GuestSyncExit::HypervisorCall {
        function,
        argument,
        extra,
    };
    let mut mismatch_pc = 0x8200;
    let mut mismatch_pstate = registers::SPSR_EL1H_AND_DAIF;
    if apply_guest_sync_action(
        hvc_exit,
        &mut general,
        &mut mismatch_pc,
        &mut mismatch_pstate,
        GuestSyncAction::WriteRegister {
            register: 1,
            value: 0,
            advance: false,
        },
    ) || mismatch_pc != 0x8200
    {
        return false;
    }
    let mut hvc_pc = 0x8200;
    let mut hvc_pstate = registers::SPSR_EL1H_AND_DAIF;
    let hvc_valid = apply_guest_sync_action(
        hvc_exit,
        &mut general,
        &mut hvc_pc,
        &mut hvc_pstate,
        emulate_hypercall(function, argument),
    ) && general[0] == registers::SMCCC_VERSION_1_1
        && hvc_pc == 0x8200
        && hvc_pstate == registers::SPSR_EL1H_AND_DAIF;
    let Some(wfi) = decode_guest_sync(
        registers::ESR_EC_WFX << registers::ESR_EC_SHIFT,
        &general,
        0x8300,
        registers::SPSR_EL1H_AND_DAIF,
    ) else {
        return false;
    };
    let Some(wfe) = decode_guest_sync(
        (registers::ESR_EC_WFX << registers::ESR_EC_SHIFT) | registers::ESR_WFX_TI_WFE,
        &general,
        0x8400,
        registers::SPSR_EL1H_AND_DAIF,
    ) else {
        return false;
    };
    let mut wfi_pc = 0x8300;
    let mut wfe_pc = 0x8400;
    hvc_valid
        && wfi == GuestSyncExit::Wait(WaitInstruction::Interrupt)
        && wfe == GuestSyncExit::Wait(WaitInstruction::Event)
        && apply_guest_sync_action(
            wfi,
            &mut general,
            &mut wfi_pc,
            &mut hvc_pstate,
            GuestSyncAction::Wait,
        )
        && apply_guest_sync_action(
            wfe,
            &mut general,
            &mut wfe_pc,
            &mut hvc_pstate,
            GuestSyncAction::Advance,
        )
        && wfi_pc == 0x8304
        && wfe_pc == 0x8404
}

fn validate_guest_memory_fault_decoder() -> bool {
    let write_syndrome = (registers::ESR_EC_DATA_ABORT_LOWER << registers::ESR_EC_SHIFT)
        | registers::ESR_ABORT_TRANSLATION_FAULT_LEVEL3
        | registers::ESR_DATA_ABORT_WNR
        | registers::ESR_DATA_ABORT_S1PTW;
    let write_valid = decode_guest_memory_fault(write_syndrome, 0x4321_0abc)
        == Some(GuestMemoryFault::new(
            GuestPhysicalAddress::new(0x4321_0abc),
            MemoryAccess::Write,
            true,
        ));
    let execute_syndrome = (registers::ESR_EC_INSTRUCTION_ABORT_LOWER << registers::ESR_EC_SHIFT)
        | registers::ESR_ABORT_PERMISSION_FAULT_LEVEL3;
    let execute_valid = decode_guest_memory_fault(execute_syndrome, 0x8000_1000)
        == Some(GuestMemoryFault::new(
            GuestPhysicalAddress::new(0x8000_1000),
            MemoryAccess::Execute,
            false,
        ));
    let read_syndrome = (registers::ESR_EC_DATA_ABORT_LOWER << registers::ESR_EC_SHIFT)
        | registers::ESR_ABORT_TRANSLATION_FAULT_LEVEL0;
    let read_valid = decode_guest_memory_fault(read_syndrome, 0x9000_2000)
        == Some(GuestMemoryFault::new(
            GuestPhysicalAddress::new(0x9000_2000),
            MemoryAccess::Read,
            false,
        ));
    let unrelated_rejected = decode_guest_memory_fault(
        registers::ESR_EC_SYSTEM_REGISTER << registers::ESR_EC_SHIFT,
        0,
    )
    .is_none();
    let unsupported_fault_rejected = decode_guest_memory_fault(
        registers::ESR_EC_DATA_ABORT_LOWER << registers::ESR_EC_SHIFT,
        0,
    )
    .is_none();
    write_valid && execute_valid && read_valid && unrelated_rejected && unsupported_fault_rejected
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GuestMmioCompletion {
    operation: MmioOperation,
    target: u8,
    width: AccessWidth,
    sign_extend: bool,
    register_64_bit: bool,
}

/// Typed completion of a suspended guest device instruction. A system-register
/// DIR is not represented as a fabricated memory access.
#[derive(Clone, Copy, Debug)]
pub(crate) enum GuestDeviceCompletion {
    Mmio(GuestMmioCompletion),
    DeactivateInterrupt,
}

impl GuestDeviceCompletion {
    pub(crate) fn apply(self, general: &mut [u64; 31], pc: &mut u64, action: MmioAction) -> bool {
        match self {
            Self::Mmio(completion) => completion.apply(general, pc, action),
            Self::DeactivateInterrupt if action == MmioAction::CompleteWrite => {
                advance(pc);
                true
            }
            Self::DeactivateInterrupt => false,
        }
    }
}

impl GuestMmioCompletion {
    pub(crate) fn apply(
        self,
        general: &mut [u64; 31],
        program_counter: &mut u64,
        action: MmioAction,
    ) -> bool {
        let value = match (self.operation, action) {
            (MmioOperation::Read, MmioAction::CompleteRead(value)) => Some(value),
            (MmioOperation::Write(_), MmioAction::CompleteWrite) => None,
            (_, MmioAction::Unhandled | MmioAction::Stop | MmioAction::Deferred)
            | (MmioOperation::Read, MmioAction::CompleteWrite)
            | (MmioOperation::Write(_), MmioAction::CompleteRead(_)) => return false,
        };
        if let Some(mut value) = value {
            let bits = self.width.bytes() * 8;
            if bits < u64::BITS as usize {
                value &= (1u64 << bits) - 1;
            }
            if self.sign_extend {
                let shift = 64 - bits;
                value = ((value << shift) as i64 >> shift) as u64;
            }
            if !self.register_64_bit {
                value &= u64::from(u32::MAX);
            }
            write_general(general, self.target, value);
        }
        advance(program_counter);
        true
    }
}

fn read_general(general: &[u64; 31], index: u8) -> u64 {
    match general.get(index as usize) {
        Some(value) => *value,
        None => 0,
    }
}

pub(crate) fn write_general(general: &mut [u64; 31], index: u8, value: u64) {
    if let Some(target) = general.get_mut(index as usize) {
        *target = value;
    }
}

pub(crate) fn advance(program_counter: &mut u64) {
    *program_counter = program_counter.wrapping_add(registers::AARCH64_INSTRUCTION_SIZE);
}

pub(crate) fn apply_guest_sync_action(
    exit: GuestSyncExit,
    general: &mut [u64; 31],
    program_counter: &mut u64,
    processor_state: &mut u64,
    action: GuestSyncAction,
) -> bool {
    if !guest_sync_action_matches(exit, action) {
        return false;
    }
    match action {
        GuestSyncAction::Advance => {
            advance(program_counter);
            true
        }
        GuestSyncAction::WriteRegister {
            register,
            value,
            advance: should_advance,
        } => {
            write_general(general, register, value);
            if should_advance {
                advance(program_counter);
            }
            true
        }
        GuestSyncAction::InjectUndefined {
            program_counter: next_program_counter,
            processor_state: next_processor_state,
        } => {
            *program_counter = next_program_counter;
            *processor_state = next_processor_state;
            true
        }
        GuestSyncAction::Wait => {
            advance(program_counter);
            true
        }
        GuestSyncAction::FirmwareWait => true,
        GuestSyncAction::Stop(_) | GuestSyncAction::InterruptAccess => false,
    }
}

pub(crate) fn guest_sync_action_matches(exit: GuestSyncExit, action: GuestSyncAction) -> bool {
    if action == GuestSyncAction::FirmwareWait {
        return matches!(exit, GuestSyncExit::HypervisorCall { .. });
    }
    if matches!(action, GuestSyncAction::Stop(_)) {
        return true;
    }
    match (exit, action) {
        (
            GuestSyncExit::SystemRegister(SystemRegisterExit {
                encoding: registers::SYSREG_ICC_DIR_EL1,
                direction: Direction::Write,
                ..
            }),
            GuestSyncAction::InterruptAccess,
        ) => true,
        (
            GuestSyncExit::SystemRegister(SystemRegisterExit {
                target,
                direction: Direction::Read,
                ..
            }),
            GuestSyncAction::WriteRegister {
                register,
                advance: true,
                ..
            },
        ) => register == target,
        (
            GuestSyncExit::SystemRegister(SystemRegisterExit {
                direction: Direction::Write,
                ..
            }),
            GuestSyncAction::Advance | GuestSyncAction::InjectUndefined { .. },
        )
        | (
            GuestSyncExit::SystemRegister(SystemRegisterExit {
                direction: Direction::Read,
                ..
            }),
            GuestSyncAction::InjectUndefined { .. },
        )
        | (GuestSyncExit::Wait(WaitInstruction::Event), GuestSyncAction::Advance)
        | (GuestSyncExit::Wait(WaitInstruction::Interrupt), GuestSyncAction::Wait)
        | (GuestSyncExit::Undefined(_), GuestSyncAction::InjectUndefined { .. }) => true,
        (
            GuestSyncExit::HypervisorCall { .. } | GuestSyncExit::SecureMonitorCall,
            GuestSyncAction::WriteRegister {
                register: 0,
                advance: false,
                ..
            },
        ) => true,
        _ => false,
    }
}

pub(crate) fn decode_guest_mmio_access(
    syndrome: u64,
    physical_address: u64,
    general: &[u64; 31],
) -> Option<(MmioAccess, GuestMmioCompletion)> {
    if (syndrome >> registers::ESR_EC_SHIFT) & registers::ESR_EC_MASK
        != registers::ESR_EC_DATA_ABORT_LOWER
        || syndrome & registers::ESR_DATA_ABORT_ISV == 0
        || syndrome & registers::ESR_DATA_ABORT_S1PTW != 0
    {
        return None;
    }
    let bytes = 1usize
        << ((syndrome >> registers::ESR_DATA_ABORT_SAS_SHIFT) & registers::ESR_DATA_ABORT_SAS_MASK);
    let width = AccessWidth::from_bytes(bytes)?;
    let target = ((syndrome >> registers::ESR_DATA_ABORT_SRT_SHIFT)
        & registers::ESR_DATA_ABORT_SRT_MASK) as u8;
    let mask = if bytes == 8 {
        u64::MAX
    } else {
        (1u64 << (bytes * 8)) - 1
    };
    let operation = if syndrome & registers::ESR_DATA_ABORT_WNR != 0 {
        MmioOperation::Write(read_general(general, target) & mask)
    } else {
        MmioOperation::Read
    };
    let access = MmioAccess::new(
        GuestPhysicalAddress::new(physical_address),
        width,
        operation,
    );
    Some((
        access,
        GuestMmioCompletion {
            operation,
            target,
            width,
            sign_extend: syndrome & registers::ESR_DATA_ABORT_SSE != 0,
            register_64_bit: syndrome & registers::ESR_DATA_ABORT_SF != 0,
        },
    ))
}

/// Decodes an owned, recoverable stage-2 fault from an `AArch64` syndrome.
///
/// The returned value carries no reference to the exception frame. Raw ESR and
/// HPFAR encodings remain private to the `AArch64` backend.
pub(crate) fn decode_guest_memory_fault(
    syndrome: u64,
    physical_address: u64,
) -> Option<GuestMemoryFault> {
    let exception_class = (syndrome >> registers::ESR_EC_SHIFT) & registers::ESR_EC_MASK;
    if !matches!(
        exception_class,
        registers::ESR_EC_INSTRUCTION_ABORT_LOWER | registers::ESR_EC_DATA_ABORT_LOWER
    ) {
        return None;
    }
    let fault_status = syndrome & registers::ESR_ABORT_FSC_MASK;
    let translation_fault = (registers::ESR_ABORT_TRANSLATION_FAULT_LEVEL0
        ..=registers::ESR_ABORT_TRANSLATION_FAULT_LEVEL3)
        .contains(&fault_status);
    let permission_fault = (registers::ESR_ABORT_PERMISSION_FAULT_LEVEL0
        ..=registers::ESR_ABORT_PERMISSION_FAULT_LEVEL3)
        .contains(&fault_status);
    if !translation_fault && !permission_fault {
        return None;
    }
    let access = if exception_class == registers::ESR_EC_INSTRUCTION_ABORT_LOWER {
        MemoryAccess::Execute
    } else if syndrome & registers::ESR_DATA_ABORT_WNR != 0 {
        MemoryAccess::Write
    } else {
        MemoryAccess::Read
    };
    Some(GuestMemoryFault::new(
        GuestPhysicalAddress::new(physical_address),
        access,
        exception_class == registers::ESR_EC_DATA_ABORT_LOWER
            && syndrome & registers::ESR_DATA_ABORT_S1PTW != 0,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Direction {
    Write,
    Read,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Access {
    encoding: Encoding,
    target: u8,
    direction: Direction,
}

pub(crate) fn decode_guest_sync(
    syndrome: u64,
    general: &[u64; 31],
    program_counter: u64,
    processor_state: u64,
) -> Option<GuestSyncExit> {
    let undefined = UndefinedExit {
        program_counter,
        processor_state,
        syndrome,
    };
    Some(
        match (syndrome >> registers::ESR_EC_SHIFT) & registers::ESR_EC_MASK {
            registers::ESR_EC_SYSTEM_REGISTER => {
                let access = decode_access(syndrome);
                GuestSyncExit::SystemRegister(SystemRegisterExit {
                    encoding: access.encoding,
                    target: access.target,
                    direction: access.direction,
                    value: match access.direction {
                        Direction::Write => read_general(general, access.target),
                        Direction::Read => 0,
                    },
                    undefined,
                })
            }
            registers::ESR_EC_HVC64 => GuestSyncExit::HypervisorCall {
                function: read_general(general, 0),
                argument: read_general(general, 1),
                extra: [read_general(general, 2), read_general(general, 3)],
            },
            registers::ESR_EC_SMC64 => GuestSyncExit::SecureMonitorCall,
            registers::ESR_EC_WFX => {
                GuestSyncExit::Wait(if syndrome & registers::ESR_WFX_TI_WFE != 0 {
                    WaitInstruction::Event
                } else {
                    WaitInstruction::Interrupt
                })
            }
            // Abort classes are completed only through memory or MMIO policy.
            registers::ESR_EC_INSTRUCTION_ABORT_LOWER | registers::ESR_EC_DATA_ABORT_LOWER => {
                return None;
            }
            _ => GuestSyncExit::Undefined(undefined),
        },
    )
}

pub(crate) fn handle_guest_sync(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    exit: GuestSyncExit,
) -> GuestSyncAction {
    match exit {
        GuestSyncExit::SystemRegister(exit) => {
            emulate_system_register(context, vcpu_id, interrupts, exit)
        }
        GuestSyncExit::HypervisorCall {
            function, argument, ..
        } => emulate_hypercall(function, argument),
        GuestSyncExit::SecureMonitorCall => GuestSyncAction::WriteRegister {
            register: 0,
            value: registers::SMCCC_NOT_SUPPORTED as u32 as u64,
            advance: false,
        },
        GuestSyncExit::Wait(WaitInstruction::Event) => GuestSyncAction::Advance,
        GuestSyncExit::Wait(WaitInstruction::Interrupt) => GuestSyncAction::Wait,
        GuestSyncExit::Undefined(exit) => inject_undefined(context, exit),
    }
}

fn emulate_hypercall(function: u64, argument: u64) -> GuestSyncAction {
    let result = match function {
        registers::SMCCC_VERSION => registers::SMCCC_VERSION_1_1,
        registers::SMCCC_ARCH_FEATURES => registers::SMCCC_NOT_SUPPORTED,
        registers::PSCI_VERSION => registers::PSCI_VERSION_1_0,
        registers::PSCI_MIGRATE_INFO_TYPE => registers::PSCI_TOS_NOT_PRESENT,
        registers::PSCI_FEATURES => match argument {
            registers::PSCI_VERSION
            | registers::PSCI_FEATURES
            | registers::PSCI_MIGRATE_INFO_TYPE
            | 0x8400_0002
            | 0x8400_0003
            | 0xc400_0003
            | 0x8400_0004
            | 0xc400_0004
            | 0x8400_0008
            | 0x8400_0009 => 0,
            _ => registers::SMCCC_NOT_SUPPORTED,
        },
        _ => registers::SMCCC_NOT_SUPPORTED,
    };
    // HVC and SMC save the architectural return address in ELR_EL2. Unlike
    // trapped instructions such as WFx and system-register accesses, the
    // exception-generating instruction has already been consumed.
    GuestSyncAction::WriteRegister {
        register: 0,
        value: result as u32 as u64,
        advance: false,
    }
}

fn emulate_system_register(
    context: &mut VcpuContext,
    vcpu_id: u32,
    interrupts: &VmInterruptController,
    exit: SystemRegisterExit,
) -> GuestSyncAction {
    if exit.encoding == registers::SYSREG_ICC_DIR_EL1 && exit.direction == Direction::Write {
        return match crate::arch::aarch64::vm::vcpu::deactivate_guest_interrupt(
            context, vcpu_id, interrupts, exit.value, false,
        ) {
            Ok(true) => GuestSyncAction::InterruptAccess,
            Ok(false) => GuestSyncAction::Advance,
            Err(error) => GuestSyncAction::Stop(GuestSyncFailure::VirtualInterrupt(error)),
        };
    }
    if matches!(
        exit.encoding,
        registers::SYSREG_ICC_PMR_EL1
            | registers::SYSREG_ICC_CTLR_EL1
            | registers::SYSREG_ICC_RPR_EL1
    ) {
        let write = (exit.direction == Direction::Write).then_some(exit.value);
        return match crate::arch::aarch64::vm::vcpu::access_common_register(
            context,
            exit.encoding,
            write,
        ) {
            Ok(Some(value)) if write.is_none() => GuestSyncAction::WriteRegister {
                register: exit.target,
                value,
                advance: true,
            },
            Ok(Some(_)) => GuestSyncAction::Advance,
            Ok(None) => inject_undefined(context, exit.undefined),
            Err(error) => GuestSyncAction::Stop(GuestSyncFailure::VirtualInterrupt(error)),
        };
    }
    match exit.direction {
        Direction::Read => match read_virtual_register(context, vcpu_id, exit.encoding) {
            Some(value) => GuestSyncAction::WriteRegister {
                register: exit.target,
                value,
                advance: true,
            },
            None => inject_undefined(context, exit.undefined),
        },
        Direction::Write => {
            if matches!(
                exit.encoding,
                registers::SYSREG_ICC_SGI1R_EL1
                    | registers::SYSREG_ICC_SGI0R_EL1
                    | registers::SYSREG_ICC_ASGI1R_EL1
            ) {
                // With a single security state, SGI1R may generate either
                // configured group; SGI0R and ASGI1R only generate Group0.
                let group = (exit.encoding != registers::SYSREG_ICC_SGI1R_EL1)
                    .then_some(hyper_vm_models::arm::gic::InterruptGroup::Group0);
                return software_interrupt_completion(
                    crate::arch::aarch64::vm::vcpu::deliver_software_interrupt_group(
                        context, vcpu_id, interrupts, exit.value, group,
                    ),
                );
            }
            if write_virtual_register(exit.encoding, exit.value) {
                GuestSyncAction::Advance
            } else {
                inject_undefined(context, exit.undefined)
            }
        }
    }
}

fn software_interrupt_completion(
    result: Result<(), crate::arch::aarch64::vm::vcpu::Error>,
) -> GuestSyncAction {
    match result {
        Ok(()) => GuestSyncAction::Advance,
        Err(error) => GuestSyncAction::Stop(GuestSyncFailure::VirtualInterrupt(error)),
    }
}

fn decode_access(esr: u64) -> Access {
    Access {
        encoding: Encoding::from_esr(esr),
        target: ((esr >> registers::ESR_SYSREG_RT_SHIFT) & registers::ESR_SYSREG_RT_MASK) as u8,
        direction: if esr & registers::ESR_SYSREG_DIRECTION_READ == 0 {
            Direction::Write
        } else {
            Direction::Read
        },
    }
}

fn read_virtual_register(_context: &VcpuContext, vcpu_id: u32, encoding: Encoding) -> Option<u64> {
    let model = crate::arch::aarch64::vm::cpu_model::frozen();
    match encoding {
        registers::SYSREG_MIDR_EL1 => Some(model.midr()),
        registers::SYSREG_MPIDR_EL1 => Some(virtual_mpidr(vcpu_id)),
        registers::SYSREG_REVIDR_EL1 => Some(model.revidr()),
        registers::SYSREG_ID_AA64PFR0_EL1 => Some(model.pfr0()),
        registers::SYSREG_ID_AA64PFR1_EL1
        | registers::SYSREG_ID_AA64PFR2_EL1
        | registers::SYSREG_ID_AA64FPFR0_EL1
        | registers::SYSREG_ID_AA64DFR1_EL1
        | registers::SYSREG_ID_AA64AFR0_EL1
        | registers::SYSREG_ID_AA64AFR1_EL1
        | registers::SYSREG_ID_AA64ISAR3_EL1
        | registers::SYSREG_ID_AA64MMFR3_EL1
        | registers::SYSREG_ID_AA64MMFR4_EL1
        | registers::SYSREG_ID_AA64ZFR0_EL1
        | registers::SYSREG_ID_AA64SMFR0_EL1 => Some(0),
        registers::SYSREG_ID_AA64DFR0_EL1 => Some(registers::ID_AA64DFR0_GUEST_BASE),
        registers::SYSREG_ID_AA64ISAR0_EL1 => Some(model.isar0()),
        registers::SYSREG_ID_AA64ISAR1_EL1 => Some(model.isar1()),
        registers::SYSREG_ID_AA64ISAR2_EL1 => Some(model.isar2()),
        registers::SYSREG_ID_AA64MMFR0_EL1 => Some(model.mmfr0()),
        registers::SYSREG_ID_AA64MMFR1_EL1 => Some(model.mmfr1()),
        registers::SYSREG_ID_AA64MMFR2_EL1 => Some(model.mmfr2()),
        registers::SYSREG_CTR_EL0 => Some(model.ctr()),
        registers::SYSREG_DCZID_EL0 => Some(model.dczid()),
        registers::SYSREG_CNTFRQ_EL0 => Some(model.cntfrq()),
        registers::SYSREG_CNTPCT_EL0 => Some(read_cntvct_el0()),
        registers::SYSREG_ACTLR_EL1 => Some(0),
        _ => None,
    }
}

fn write_virtual_register(encoding: Encoding, _value: u64) -> bool {
    // ACTLR_EL1 is architecturally implementation-defined. RAZ/WI prevents a
    // guest from depending on host-specific auxiliary controls.
    encoding == registers::SYSREG_ACTLR_EL1
}

fn inject_undefined(context: &mut VcpuContext, exit: UndefinedExit) -> GuestSyncAction {
    let syndrome = exit.syndrome & registers::ESR_IL;
    let vector_offset = match exit.processor_state & registers::SPSR_M_MASK {
        registers::SPSR_EL0T => registers::VECTOR_LOWER_EL_AARCH64,
        registers::SPSR_EL1T => registers::VECTOR_CURRENT_EL_SP0,
        _ => registers::VECTOR_CURRENT_EL_SPX,
    };

    context.esr_el1 = syndrome;
    context.far_el1 = 0;
    context.elr_el1 = exit.program_counter;
    context.spsr_el1 = exit.processor_state;
    // SAFETY: The active-vCPU bridge guarantees this is the guest whose EL1
    // bank is live on the current CPU.
    let live_vbar =
        unsafe { load_undefined_exception(syndrome, exit.program_counter, exit.processor_state) };
    context.vbar_el1 = live_vbar;
    GuestSyncAction::InjectUndefined {
        program_counter: live_vbar.wrapping_add(vector_offset),
        processor_state: (exit.processor_state & !registers::SPSR_MODE_AND_DAIF_MASK)
            | registers::SPSR_EL1H_AND_DAIF,
    }
}

unsafe fn load_undefined_exception(syndrome: u64, elr: u64, spsr: u64) -> u64 {
    let vbar: u64;
    // SAFETY: Guest execution is stopped and the caller guarantees the live
    // EL12 exception bank belongs to the active vCPU.
    unsafe {
        asm!(
            "mrs {vbar}, S3_5_C12_C0_0",
            "msr S3_5_C5_C2_0, {esr}",
            "msr S3_5_C6_C0_0, xzr",
            "msr S3_5_C4_C0_1, {elr}",
            "msr S3_5_C4_C0_0, {spsr}",
            vbar = out(reg) vbar,
            esr = in(reg) syndrome,
            elr = in(reg) elr,
            spsr = in(reg) spsr,
            options(nostack, preserves_flags)
        );
    }
    vbar
}

const fn virtual_mpidr(vcpu_id: u32) -> u64 {
    let id = vcpu_id as u64;
    (id & registers::MPIDR_AFF0_TO_2_MASK)
        | ((id & registers::MPIDR_LINEAR_AFF3_MASK) << registers::MPIDR_AFF3_FROM_LINEAR_ID_SHIFT)
}

macro_rules! read_register {
    ($function:ident, $register:literal) => {
        fn $function() -> u64 {
            let value: u64;
            // SAFETY: The named register is readable at EL2 and the operation
            // has no memory side effects.
            unsafe {
                asm!(
                    concat!("mrs {value}, ", $register),
                    value = out(reg) value,
                    options(nomem, nostack, preserves_flags)
                );
            }
            value
        }
    };
}

read_register!(read_cntvct_el0, "CNTVCT_EL0");
