// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! RISC-V guest-exit capture, hardware emulation, and private completion.
//!
//! A trap frame belongs to architecture entry. [`dispatch`] copies the
//! guest-visible exit into fixed-width owned state, ends the frame borrow,
//! invokes the registered kernel service, and only then applies the returned
//! action. Neither the raw frame nor a vCPU-context borrow crosses that call.

use core::arch::asm;

use {
    hyper_vm_models::exit::GuestMemoryFault, hyper_vm_models::exit::GuestPhysicalAddress,
    hyper_vm_models::exit::MemoryAccess, hyper_vm_models::exit::MemoryFaultAction,
};

use {crate::arch::riscv64::VcpuContext, crate::arch::riscv64::VmInterruptController};
use {
    crate::arch::riscv64::context::GuestRunExit,
    crate::arch::riscv64::context::GuestSynchronousTerminal,
    crate::arch::riscv64::context::GuestTerminalCause,
    crate::arch::riscv64::context::GuestTerminalExit,
    crate::arch::riscv64::context::GuestWaitReason,
};
use {
    hyper_vm_models::exit::MmioAccess, hyper_vm_models::exit::MmioAction,
    hyper_vm_models::exit::MmioOperation,
};
use {
    hyper_vm_models::riscv64::instruction::GuestInstruction,
    hyper_vm_models::riscv64::instruction::Htinst,
    hyper_vm_models::riscv64::instruction::MmioRegister,
    hyper_vm_models::riscv64::instruction::classify_htinst,
    hyper_vm_models::riscv64::instruction::decode_mmio,
};

pub(crate) enum Dispatch {
    Resume,
    Stop(GuestRunExit),
}

fn terminal(
    frame: &crate::arch::riscv64::exception::TrapFrame,
    cause: GuestTerminalCause,
) -> Dispatch {
    Dispatch::Stop(GuestRunExit::Terminal(GuestTerminalExit::from_frame(
        frame, cause,
    )))
}

#[repr(C)]
#[derive(Default)]
struct Parcel {
    value: u64,
    cause: u64,
    trap_value: u64,
    guest_address: u64,
    instruction: u64,
}
unsafe extern "C" {
    fn riscv64_fetch_guest_parcel(
        address: u64,
        context: *const (),
        result: *mut Parcel,
        hstatus: u64,
    ) -> u64;
}

enum Fetch {
    Instruction(u32),
    Retry,
    Failed,
}

fn fetch_instruction(frame: &crate::arch::riscv64::exception::TrapFrame) -> Fetch {
    let context = frame.guest_context_address() as *const VcpuContext;
    if context.is_null() || !context.is_aligned() || frame.sepc & 1 != 0 {
        return Fetch::Failed;
    }
    let mut instruction = 0u32;
    for parcel_index in 0..2 {
        let mut parcel = Parcel::default();
        let Some(address) = frame.sepc.checked_add(parcel_index * 2) else {
            return Fetch::Failed;
        };
        // SAFETY: Trap entry published this pinned owner, captured its banks,
        // and retained its HGATP lease. Assembly contains faults and restores
        // every host CSR before returning; no context borrow crosses policy.
        let failed = unsafe {
            riscv64_fetch_guest_parcel(address, context.cast(), &mut parcel, frame.hstatus)
        };
        if failed != 0 {
            if let Some(fault) = decode_guest_memory_fault(
                parcel.cause,
                parcel.trap_value,
                parcel.guest_address,
                parcel.instruction,
            ) {
                // HLVX reports load faults, but its explicit access requires
                // execute permission. Implicit VS page-table walks still use
                // their separately decoded read/write permission.
                let fault = if fault.during_guest_page_walk() {
                    fault
                } else {
                    GuestMemoryFault::new(fault.address(), MemoryAccess::Execute, false)
                };
                if matches!(
                    crate::arch::vm::dispatch_memory_fault(fault),
                    MemoryFaultAction::Retry
                ) {
                    return Fetch::Retry;
                }
            }
            return Fetch::Failed;
        }
        // HLVX obeys the selected guest data endian mode; instruction parcels
        // are always little-endian, including when the guest data bank is BE.
        // SAFETY: The short read occurs only after the probe restored HS state.
        let user_big_endian = unsafe { (*context).vsstatus & (1 << 6) != 0 };
        let big_endian = if frame.hstatus & (1 << 8) != 0 {
            frame.hstatus & (1 << 5) != 0
        } else {
            user_big_endian
        };
        let value = if big_endian {
            (parcel.value as u16).swap_bytes()
        } else {
            parcel.value as u16
        };
        instruction |= u32::from(value) << (parcel_index * 16);
        if parcel_index == 0 && instruction & 3 != 3 {
            break;
        }
    }
    Fetch::Instruction(instruction)
}

const CAUSE_VIRTUAL_SUPERVISOR_ECALL: u64 = 10;
const CAUSE_INSTRUCTION_GUEST_PAGE_FAULT: u64 = 20;
const CAUSE_LOAD_GUEST_PAGE_FAULT: u64 = 21;
const CAUSE_VIRTUAL_INSTRUCTION: u64 = 22;
const CAUSE_STORE_GUEST_PAGE_FAULT: u64 = 23;
const HVIP_VSTIP: usize = 1 << 6;

const SBI_EXT_BASE: u64 = 0x10;
const SBI_EXT_TIME: u64 = 0x5449_4d45;
const SBI_SUCCESS: u64 = 0;
const SBI_NOT_SUPPORTED: u64 = (-2isize) as u64;

const SYSTEM_OPCODE: u32 = 0x73;
const WFI: u32 = 0x1050_0073;
const CSR_SIE: u32 = 0x104;
const CSR_SIP: u32 = 0x144;
const CSR_SCOUNTEREN: u32 = 0x106;
const CSR_SENVCFG: u32 = 0x10a;

/// One owned RISC-V synchronous guest exit presented to kernel policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestSyncExit {
    VirtualInstruction(VirtualInstruction),
    SupervisorCall(SupervisorCall),
    Unsupported(UnsupportedGuestExit),
    IllegalInstruction(IllegalInstruction),
}

impl GuestSyncExit {
    /// Returns the byte carried by a legacy debug-console operation.
    pub const fn legacy_console_byte(self) -> Option<u8> {
        match self {
            Self::SupervisorCall(call) => call.legacy_console_byte(),
            Self::VirtualInstruction(_) | Self::Unsupported(_) | Self::IllegalInstruction(_) => {
                None
            }
        }
    }
}

/// Guest-visible illegal instruction forwarded through the saved VS trap bank.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IllegalInstruction {
    program_counter: u64,
    instruction: u64,
    supervisor: bool,
}

/// A virtual instruction which can be completed by the RISC-V backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtualInstruction {
    WaitForInterrupt,
    Csr(CsrInstruction),
}

/// One validated virtual CSR access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CsrInstruction {
    register: VirtualCsr,
    operation: CsrOperation,
    source: u64,
}

/// Guest-visible virtual CSRs implemented by the initial backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VirtualCsr {
    InterruptEnable,
    InterruptPending,
    CounterEnable,
    EnvironmentConfiguration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CsrOperation {
    Write,
    SetBits,
    ClearBits,
}

/// One copied supervisor binary interface invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisorCall {
    extension: u64,
    function: u64,
    arguments: [u64; 6],
}

impl SupervisorCall {
    /// Returns the byte carried by the legacy debug-console call.
    ///
    /// Console routing is kernel VM policy; architecture code only identifies
    /// the ABI operation and later completes its register convention.
    pub const fn legacy_console_byte(self) -> Option<u8> {
        if self.extension == 1 {
            Some(self.arguments[0] as u8)
        } else {
            None
        }
    }
}

/// Diagnostic details for a synchronous exit which cannot be resumed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsupportedGuestExit {
    pub(crate) cause: u64,
    pub(crate) trap_value: u64,
    pub(crate) transformed_instruction: u64,
    pub(crate) reason: UnsupportedReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsupportedReason {
    GuestPhysicalAddressUnavailable,
    SynchronousCause,
}

/// The exhaustive architecture completion returned by VM-exit policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum GuestSyncAction {
    ResumeVirtualInstruction {
        value: Option<u64>,
    },
    CompleteSupervisorCall(SupervisorCallResult),
    ProgramTimer {
        deadline: u64,
        result: SupervisorCallResult,
    },
    EnterSupervisorTrap {
        program_counter: u64,
    },
    Stop,
}

impl GuestSyncAction {
    /// Completes one legacy debug-console call after kernel policy emitted it.
    pub const fn complete_legacy_console() -> Self {
        Self::CompleteSupervisorCall(SupervisorCallResult::Legacy { value: SBI_SUCCESS })
    }
}

/// Register result following the legacy or modern SBI convention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorCallResult {
    /// Legacy SBI v0.1 writes only `a0`; `a1` remains guest-owned.
    Legacy { value: u64 },
    /// SBI v0.2 and later write the error to `a0` and value to `a1`.
    Modern { error: u64, value: u64 },
}

#[derive(Debug, Eq, PartialEq)]
enum CapturedExit {
    MemoryFault(GuestMemoryFault),
    Synchronous {
        exit: GuestSyncExit,
        completion: Completion,
    },
}

#[derive(Debug, Eq, PartialEq)]
enum Completion {
    VirtualInstruction { destination: usize },
    SupervisorCall(SupervisorCall),
    IllegalInstruction,
    Unsupported,
}

/// Captures, dispatches, and completes one synchronous guest exit.
///
/// Local interrupts are masked by hardware on trap entry. Capture returns only
/// copied state, so the raw frame borrow ends before either kernel service may
/// allocate or take VM locks.
pub(crate) fn dispatch(frame: &mut crate::arch::riscv64::exception::TrapFrame) -> Dispatch {
    if frame.scause == CAUSE_VIRTUAL_INSTRUCTION
        && frame.stval == 0
        && frame.hstatus & (1 << 8) != 0
    {
        match fetch_instruction(frame) {
            Fetch::Instruction(value) => frame.stval = u64::from(value),
            Fetch::Retry => return Dispatch::Resume,
            Fetch::Failed => {
                return terminal(
                    frame,
                    GuestTerminalCause::Synchronous(GuestSynchronousTerminal::Undecodable),
                );
            }
        }
    }
    let captured = capture(frame);
    let action = match &captured {
        CapturedExit::MemoryFault(fault) => {
            return match crate::arch::vm::dispatch_memory_fault(*fault) {
                MemoryFaultAction::Retry => Dispatch::Resume,
                MemoryFaultAction::ForwardToDevice => dispatch_mmio(frame, *fault),
                MemoryFaultAction::Stop => terminal(frame, GuestTerminalCause::MemoryFault),
            };
        }
        CapturedExit::Synchronous { exit, .. } => crate::arch::vm::dispatch_guest_sync(*exit),
    };
    let wfi = matches!(
        captured,
        CapturedExit::Synchronous {
            exit: GuestSyncExit::VirtualInstruction(VirtualInstruction::WaitForInterrupt),
            ..
        }
    );
    let cause = match &captured {
        CapturedExit::Synchronous {
            exit: GuestSyncExit::SupervisorCall(call),
            ..
        } => GuestSynchronousTerminal::SupervisorCall {
            extension: call.extension,
            function: call.function,
        },
        CapturedExit::Synchronous {
            exit: GuestSyncExit::Unsupported(exit),
            ..
        } => GuestSynchronousTerminal::Unsupported(*exit),
        _ => GuestSynchronousTerminal::Undecodable,
    };
    if apply(frame, captured, action) {
        if wfi {
            Dispatch::Stop(GuestRunExit::Wait(GuestWaitReason::Interrupt))
        } else {
            Dispatch::Resume
        }
    } else {
        terminal(frame, GuestTerminalCause::Synchronous(cause))
    }
}

fn dispatch_mmio(
    frame: &mut crate::arch::riscv64::exception::TrapFrame,
    fault: GuestMemoryFault,
) -> Dispatch {
    if fault.during_guest_page_walk() || fault.access() == MemoryAccess::Execute {
        return terminal(frame, GuestTerminalCause::MemoryFault);
    }
    let instruction = match classify_htinst(frame.htinst) {
        Htinst::Instruction(value) => value,
        Htinst::Unavailable => match fetch_instruction(frame) {
            Fetch::Instruction(value) => GuestInstruction::Fetched(value),
            Fetch::Retry => return Dispatch::Resume,
            Fetch::Failed => return terminal(frame, GuestTerminalCause::Mmio),
        },
        _ => return terminal(frame, GuestTerminalCause::Mmio),
    };
    let Ok(decoded) = decode_mmio(instruction, fault.access()) else {
        return terminal(frame, GuestTerminalCause::Mmio);
    };
    let alignment = decoded.width.bytes() as u64 - 1;
    if decoded.address_offset != 0
        || frame.stval & alignment != 0
        || fault.address().get() & alignment != 0
    {
        return terminal(frame, GuestTerminalCause::Mmio);
    }
    if let Some(address) = decoded.address {
        let effective = frame.general[usize::from(address.base)]
            .wrapping_add_signed(i64::from(address.displacement));
        if effective != frame.stval {
            return terminal(frame, GuestTerminalCause::Mmio);
        }
    }
    let operation = match decoded.operation {
        MmioRegister::Load { .. } => MmioOperation::Read,
        MmioRegister::Store { rs2 } => MmioOperation::Write(frame.general[usize::from(rs2)]),
    };
    let action =
        crate::arch::vm::dispatch_mmio(MmioAccess::new(fault.address(), decoded.width, operation));
    match (decoded.operation, action) {
        (MmioRegister::Load { rd, signed }, MmioAction::CompleteRead(value)) => {
            let shift = 64 - decoded.width.bytes() * 8;
            let value = if signed {
                (((value << shift) as i64) >> shift) as u64
            } else {
                (value << shift) >> shift
            };
            if rd != 0 {
                frame.general[usize::from(rd)] = value;
            }
        }
        (MmioRegister::Store { .. }, MmioAction::CompleteWrite) => {}
        _ => return terminal(frame, GuestTerminalCause::Mmio),
    }
    frame.sepc = frame
        .sepc
        .wrapping_add(u64::from(decoded.instruction_bytes));
    Dispatch::Resume
}

fn capture(frame: &crate::arch::riscv64::exception::TrapFrame) -> CapturedExit {
    if let Some(fault) =
        decode_guest_memory_fault(frame.scause, frame.stval, frame.htval, frame.htinst)
    {
        return CapturedExit::MemoryFault(fault);
    }
    if matches!(
        frame.scause,
        CAUSE_INSTRUCTION_GUEST_PAGE_FAULT
            | CAUSE_LOAD_GUEST_PAGE_FAULT
            | CAUSE_STORE_GUEST_PAGE_FAULT
    ) {
        return unsupported(frame, UnsupportedReason::GuestPhysicalAddressUnavailable);
    }
    if frame.scause == CAUSE_VIRTUAL_INSTRUCTION {
        if frame.hstatus & (1 << 8) == 0 {
            // HS-qualified supervisor/hypervisor operations trap here even
            // from VU. They must not be emulated with supervisor authority.
            return illegal_instruction(frame);
        }
        return capture_virtual_instruction(frame);
    }
    if frame.scause == CAUSE_VIRTUAL_SUPERVISOR_ECALL {
        let call = SupervisorCall {
            extension: frame.general[17],
            function: frame.general[16],
            arguments: [
                frame.general[10],
                frame.general[11],
                frame.general[12],
                frame.general[13],
                frame.general[14],
                frame.general[15],
            ],
        };
        return CapturedExit::Synchronous {
            exit: GuestSyncExit::SupervisorCall(call),
            completion: Completion::SupervisorCall(call),
        };
    }
    unsupported(frame, UnsupportedReason::SynchronousCause)
}

fn capture_virtual_instruction(frame: &crate::arch::riscv64::exception::TrapFrame) -> CapturedExit {
    // For a virtual-instruction exception, STVAL contains the trapped
    // instruction. HTINST is reserved for transformed instructions associated
    // with guest-page faults and must not be preferred here.
    let instruction = frame.stval as u32;
    if instruction == WFI {
        return CapturedExit::Synchronous {
            exit: GuestSyncExit::VirtualInstruction(VirtualInstruction::WaitForInterrupt),
            completion: Completion::VirtualInstruction { destination: 0 },
        };
    }
    let Some((instruction, destination)) = decode_csr_instruction(instruction, &frame.general)
    else {
        return illegal_instruction(frame);
    };
    CapturedExit::Synchronous {
        exit: GuestSyncExit::VirtualInstruction(VirtualInstruction::Csr(instruction)),
        completion: Completion::VirtualInstruction { destination },
    }
}

fn illegal_instruction(frame: &crate::arch::riscv64::exception::TrapFrame) -> CapturedExit {
    CapturedExit::Synchronous {
        exit: GuestSyncExit::IllegalInstruction(IllegalInstruction {
            program_counter: frame.sepc,
            instruction: frame.stval,
            supervisor: frame.hstatus & (1 << 8) != 0,
        }),
        completion: Completion::IllegalInstruction,
    }
}

fn decode_csr_instruction(
    instruction: u32,
    general: &[u64; 32],
) -> Option<(CsrInstruction, usize)> {
    if instruction & 0x7f != SYSTEM_OPCODE {
        return None;
    }
    let function = (instruction >> 12) & 7;
    let source_register = ((instruction >> 15) & 0x1f) as usize;
    let destination = ((instruction >> 7) & 0x1f) as usize;
    let register = match instruction >> 20 {
        CSR_SIE => VirtualCsr::InterruptEnable,
        CSR_SIP => VirtualCsr::InterruptPending,
        CSR_SCOUNTEREN => VirtualCsr::CounterEnable,
        CSR_SENVCFG => VirtualCsr::EnvironmentConfiguration,
        _ => return None,
    };
    let operation = match function {
        1 | 5 => CsrOperation::Write,
        2 | 6 => CsrOperation::SetBits,
        3 | 7 => CsrOperation::ClearBits,
        _ => return None,
    };
    let source = if function >= 5 {
        source_register as u64
    } else {
        general.get(source_register).copied()?
    };
    Some((
        CsrInstruction {
            register,
            operation,
            source,
        },
        destination,
    ))
}

fn unsupported(
    frame: &crate::arch::riscv64::exception::TrapFrame,
    reason: UnsupportedReason,
) -> CapturedExit {
    CapturedExit::Synchronous {
        exit: GuestSyncExit::Unsupported(UnsupportedGuestExit {
            cause: frame.scause,
            trap_value: frame.stval,
            transformed_instruction: frame.htinst,
            reason,
        }),
        completion: Completion::Unsupported,
    }
}

fn decode_guest_memory_fault(
    cause: u64,
    trap_value: u64,
    guest_physical_address: u64,
    transformed_instruction: u64,
) -> Option<GuestMemoryFault> {
    let guest_access = match cause {
        CAUSE_INSTRUCTION_GUEST_PAGE_FAULT => MemoryAccess::Execute,
        CAUSE_LOAD_GUEST_PAGE_FAULT => MemoryAccess::Read,
        CAUSE_STORE_GUEST_PAGE_FAULT => MemoryAccess::Write,
        _ => return None,
    };
    // HTVAL may be zero when the implementation omits the GPA, but zero is
    // also a valid encoded GPA. Refusing demand paging is the only safe choice.
    if guest_physical_address == 0 {
        return None;
    }
    let (access, during_guest_page_walk) = guest_page_walk_access(transformed_instruction)
        .map_or((guest_access, false), |access| (access, true));
    Some(GuestMemoryFault::new(
        GuestPhysicalAddress::new(guest_physical_fault_address(
            guest_physical_address,
            trap_value,
            during_guest_page_walk,
        )),
        access,
        during_guest_page_walk,
    ))
}

fn guest_page_walk_access(instruction: u64) -> Option<MemoryAccess> {
    // The privileged ISA requires one of these values when an implicit
    // VS-stage page-table access faults and HTVAL is nonzero. Other nonzero
    // HTINST values encode the explicit instruction which faulted.
    // https://docs.riscv.org/reference/isa/priv/hypervisor.html
    match instruction {
        0x2000 | 0x3000 => Some(MemoryAccess::Read),
        0x2020 | 0x3020 => Some(MemoryAccess::Write),
        _ => None,
    }
}

fn guest_physical_fault_address(htval: u64, stval: u64, during_guest_page_walk: bool) -> u64 {
    let low_bits = if during_guest_page_walk { 0 } else { stval & 3 };
    (htval << 2) | low_bits
}

/// Applies backend-local virtual-instruction and SBI mechanisms.
///
/// The kernel calls this only while its active-vCPU capability exclusively
/// borrows `context`. The owned exit contains no trap-frame reference.
pub(crate) fn handle_guest_sync(
    context: &mut VcpuContext,
    _vcpu_id: u32,
    _interrupts: &VmInterruptController,
    exit: GuestSyncExit,
) -> GuestSyncAction {
    match exit {
        GuestSyncExit::VirtualInstruction(instruction) => {
            emulate_virtual_instruction(context, instruction)
        }
        GuestSyncExit::SupervisorCall(call) => emulate_sbi(context, call),
        GuestSyncExit::Unsupported(_) => GuestSyncAction::Stop,
        GuestSyncExit::IllegalInstruction(exit) => inject_illegal_instruction(context, exit),
    }
}

fn inject_illegal_instruction(
    context: &mut VcpuContext,
    exit: IllegalInstruction,
) -> GuestSyncAction {
    const SIE: u64 = 1 << 1;
    const SPIE: u64 = 1 << 5;
    const SPP: u64 = 1 << 8;
    let previous = context.vsstatus;
    context.vsstatus = (previous & !(SIE | SPIE | SPP))
        | ((previous & SIE) << 4)
        | if exit.supervisor { SPP } else { 0 };
    context.vsepc = exit.program_counter;
    context.vscause = 2;
    context.vstval = exit.instruction;
    context.supervisor = 1;
    // This is a guest virtual PC, never a host pointer. The hardware-captured
    // WARL VSTVEC supplies the base; synchronous exceptions ignore vectored
    // mode. An inaccessible base faults inside the guest translation regime.
    GuestSyncAction::EnterSupervisorTrap {
        program_counter: context.vstvec & !3,
    }
}

fn emulate_virtual_instruction(
    context: &mut VcpuContext,
    instruction: VirtualInstruction,
) -> GuestSyncAction {
    let VirtualInstruction::Csr(instruction) = instruction else {
        return GuestSyncAction::ResumeVirtualInstruction { value: None };
    };
    let old = match instruction.register {
        VirtualCsr::InterruptEnable => context.vsie,
        VirtualCsr::InterruptPending => read_guest_vsip(context),
        VirtualCsr::CounterEnable => context.scounteren,
        VirtualCsr::EnvironmentConfiguration => context.senvcfg,
    };
    let new_value = match instruction.operation {
        CsrOperation::Write => Some(instruction.source),
        CsrOperation::SetBits if instruction.source != 0 => Some(old | instruction.source),
        CsrOperation::ClearBits if instruction.source != 0 => Some(old & !instruction.source),
        CsrOperation::SetBits | CsrOperation::ClearBits => None,
    };
    if let Some(value) = new_value {
        match instruction.register {
            VirtualCsr::InterruptEnable => context.vsie = value & 0x222,
            VirtualCsr::InterruptPending => write_guest_vsip(context, value),
            VirtualCsr::CounterEnable => context.scounteren = sanitize_scounteren(value),
            VirtualCsr::EnvironmentConfiguration => context.senvcfg = sanitize_senvcfg(value),
        }
    }
    GuestSyncAction::ResumeVirtualInstruction { value: Some(old) }
}

fn read_guest_vsip(context: &VcpuContext) -> u64 {
    let physical: u64;
    // SAFETY: TIME is readable in HS on every admitted reference-platform hart.
    unsafe {
        asm!("rdtime {value}", value = out(reg) physical, options(nomem, nostack));
    }
    let timer = physical.wrapping_add(context.virtual_count_offset()) >= context.vstimecmp;
    (context.hvip >> 1) | (u64::from(timer) << 5)
}

fn write_guest_vsip(context: &mut VcpuContext, value: u64) {
    const VSIP_SSIP: u64 = 1 << 1;
    const HVIP_VSSIP: u64 = 1 << 2;
    context.hvip &= !HVIP_VSSIP;
    if value & VSIP_SSIP != 0 {
        context.hvip |= HVIP_VSSIP;
    }
}

fn sanitize_scounteren(value: u64) -> u64 {
    let host: u64;
    let accepted: u64;
    // SAFETY: The guest trap entry restored the host value before Rust ran.
    // Local interrupts are masked, so the temporary WARL probe cannot be
    // observed by another context on this hart, and the exact host value is
    // restored before returning.
    unsafe {
        asm!("csrr {host}, scounteren", host = out(reg) host, options(nomem, nostack));
        asm!(
            "csrw scounteren, {value}",
            "csrr {accepted}, scounteren",
            "csrw scounteren, {host}",
            host = in(reg) host,
            accepted = out(reg) accepted,
            value = in(reg) value,
            options(nomem, nostack)
        )
    };
    accepted
}

fn sanitize_senvcfg(value: u64) -> u64 {
    let host: u64;
    let accepted: u64;
    // SAFETY: This is the SENVCFG counterpart of `sanitize_scounteren`; the
    // host value is restored before any interrupt or scheduler boundary.
    unsafe {
        asm!("csrr {host}, senvcfg", host = out(reg) host, options(nomem, nostack));
        asm!(
            "csrw senvcfg, {value}",
            "csrr {accepted}, senvcfg",
            "csrw senvcfg, {host}",
            host = in(reg) host,
            accepted = out(reg) accepted,
            value = in(reg) value,
            options(nomem, nostack)
        )
    };
    accepted
}

fn emulate_sbi(context: &mut VcpuContext, call: SupervisorCall) -> GuestSyncAction {
    match call.extension {
        0 => GuestSyncAction::ProgramTimer {
            deadline: call.arguments[0],
            result: SupervisorCallResult::Legacy { value: SBI_SUCCESS },
        },
        // Legacy console output is intercepted by kernel policy before this
        // backend helper. Reaching it means the service violated its contract.
        1 => GuestSyncAction::Stop,
        2 => GuestSyncAction::CompleteSupervisorCall(SupervisorCallResult::Legacy {
            value: u64::MAX,
        }),
        3 => {
            clear_legacy_software_interrupt(context);
            GuestSyncAction::CompleteSupervisorCall(SupervisorCallResult::Legacy {
                value: SBI_SUCCESS,
            })
        }
        // Legacy remote IPI/fence and shutdown calls have no error return with
        // which to report absence. Do not silently claim their side effects.
        4..=8 => GuestSyncAction::Stop,
        _ => emulate_modern_sbi(call),
    }
}

fn emulate_modern_sbi(call: SupervisorCall) -> GuestSyncAction {
    let (error, value) = match (call.extension, call.function) {
        (SBI_EXT_BASE, 0) => (SBI_SUCCESS, 0x0000_0003),
        (SBI_EXT_BASE, 1 | 2 | 4..=6) => (SBI_SUCCESS, 0),
        (SBI_EXT_BASE, 3) => {
            let available = modern_extension_available(call.arguments[0]);
            (SBI_SUCCESS, u64::from(available))
        }
        (SBI_EXT_TIME, 0) => {
            return GuestSyncAction::ProgramTimer {
                deadline: call.arguments[0],
                result: SupervisorCallResult::Modern {
                    error: SBI_SUCCESS,
                    value: 0,
                },
            };
        }
        _ => (SBI_NOT_SUPPORTED, 0),
    };
    GuestSyncAction::CompleteSupervisorCall(SupervisorCallResult::Modern { error, value })
}

const fn modern_extension_available(extension: u64) -> bool {
    extension == SBI_EXT_TIME
}

fn clear_legacy_software_interrupt(context: &mut VcpuContext) {
    const HVIP_VSSIP: u64 = 1 << 2;
    // Trap entry has already quiesced physical HVIP. Update the authoritative
    // vCPU image so the resume path cannot reassert the cleared software IRQ.
    context.hvip &= !HVIP_VSSIP;
}

fn apply(
    frame: &mut crate::arch::riscv64::exception::TrapFrame,
    captured: CapturedExit,
    action: GuestSyncAction,
) -> bool {
    let CapturedExit::Synchronous { completion, .. } = captured else {
        return false;
    };
    match (completion, action) {
        (
            Completion::VirtualInstruction { destination },
            GuestSyncAction::ResumeVirtualInstruction { value },
        ) => {
            apply_virtual_instruction_result(&mut frame.general, destination, value);
            advance_program_counter(frame);
            true
        }
        (Completion::SupervisorCall(call), GuestSyncAction::CompleteSupervisorCall(result))
            if supervisor_completion_matches(call, result) =>
        {
            apply_supervisor_call_result(&mut frame.general, result);
            advance_program_counter(frame);
            true
        }
        (Completion::SupervisorCall(call), GuestSyncAction::ProgramTimer { deadline, result })
            if supervisor_timer_matches(call, result) =>
        {
            if !set_timer(frame, deadline) {
                return false;
            }
            apply_supervisor_call_result(&mut frame.general, result);
            advance_program_counter(frame);
            true
        }
        (
            Completion::IllegalInstruction,
            GuestSyncAction::EnterSupervisorTrap { program_counter },
        ) => {
            // The saved VS bank already describes the exception. Return into
            // VS, keeping every interrupted GPR and the faulting VSEPC intact.
            frame.sepc = program_counter;
            frame.sstatus |= 1 << 8;
            frame.hstatus |= (1 << 7) | (1 << 8);
            true
        }
        (Completion::Unsupported, GuestSyncAction::Stop) => false,
        _ => false,
    }
}

const fn supervisor_result_matches(call: SupervisorCall, result: SupervisorCallResult) -> bool {
    matches!(
        (call.extension <= 8, result),
        (true, SupervisorCallResult::Legacy { .. }) | (false, SupervisorCallResult::Modern { .. })
    )
}

const fn supervisor_completion_matches(call: SupervisorCall, result: SupervisorCallResult) -> bool {
    !supervisor_call_is_timer(call) && supervisor_result_matches(call, result)
}

const fn supervisor_timer_matches(call: SupervisorCall, result: SupervisorCallResult) -> bool {
    supervisor_call_is_timer(call) && supervisor_result_matches(call, result)
}

const fn supervisor_call_is_timer(call: SupervisorCall) -> bool {
    call.extension == 0 || (call.extension == SBI_EXT_TIME && call.function == 0)
}

fn apply_supervisor_call_result(general: &mut [u64; 32], result: SupervisorCallResult) {
    match result {
        SupervisorCallResult::Legacy { value } => general[10] = value,
        SupervisorCallResult::Modern { error, value } => {
            general[10] = error;
            general[11] = value;
        }
    }
}

fn apply_virtual_instruction_result(
    general: &mut [u64; 32],
    destination: usize,
    value: Option<u64>,
) {
    if destination != 0
        && let Some(value) = value
        && let Some(register) = general.get_mut(destination)
    {
        *register = value;
    }
}

fn advance_program_counter(frame: &mut crate::arch::riscv64::exception::TrapFrame) {
    // ECALL, WFI, and every CSR instruction accepted above are 32-bit SYSTEM
    // encodings; none has a compressed representation in these exit classes.
    advance_program_counter_value(&mut frame.sepc);
}

fn advance_program_counter_value(program_counter: &mut u64) {
    *program_counter = program_counter.wrapping_add(4);
}

fn set_timer(frame: &crate::arch::riscv64::exception::TrapFrame, deadline: u64) -> bool {
    let context = frame.guest_context_address() as *mut VcpuContext;
    if context.is_null() || !context.is_aligned() {
        return false;
    }
    // SAFETY: Assembly published the exact live context in the guest anchor.
    // Policy dispatch has returned and no Rust reference to the context was
    // retained across it. Local interrupts remain masked in this completion.
    unsafe {
        (*context).vstimecmp = deadline;
        (*context).hvip &= !(HVIP_VSTIP as u64);
    }
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationError {
    GuestAnchorState,
    CsrDecoder,
    GuestMemoryFaultDecoder,
    SupervisorCallCompletion,
    IllegalInstructionTrap,
}

pub(crate) fn validate() -> Result<(), ValidationError> {
    if !crate::arch::riscv64::context::validate_anchor_state_machine() {
        return Err(ValidationError::GuestAnchorState);
    }
    if !validate_illegal_instruction_traps() {
        return Err(ValidationError::IllegalInstructionTrap);
    }

    if guest_page_walk_access(0x3000) != Some(MemoryAccess::Read)
        || guest_page_walk_access(0x3020) != Some(MemoryAccess::Write)
        || guest_page_walk_access(0x0000_20c3).is_some()
        || guest_physical_fault_address(0x1234, 3, true) != 0x48d0
        || guest_physical_fault_address(0x1234, 3, false) != 0x48d3
        || decode_guest_memory_fault(CAUSE_LOAD_GUEST_PAGE_FAULT, 0, 0, 0).is_some()
    {
        return Err(ValidationError::GuestMemoryFaultDecoder);
    }

    let mut registers = [0u64; 32];
    registers[5] = 0x55aa;
    // csrrw x7, sie, x5
    let csr = (CSR_SIE << 20) | (5 << 15) | (1 << 12) | (7 << 7) | SYSTEM_OPCODE;
    let Some((decoded, destination)) = decode_csr_instruction(csr, &registers) else {
        return Err(ValidationError::CsrDecoder);
    };
    if decoded.register != VirtualCsr::InterruptEnable
        || decoded.operation != CsrOperation::Write
        || decoded.source != 0x55aa
        || destination != 7
    {
        return Err(ValidationError::CsrDecoder);
    }
    apply_virtual_instruction_result(&mut registers, 0, Some(0xffff));
    if registers[0] != 0 {
        return Err(ValidationError::CsrDecoder);
    }
    apply_virtual_instruction_result(&mut registers, destination, Some(0x1234));
    if registers[7] != 0x1234 {
        return Err(ValidationError::CsrDecoder);
    }
    let mut program_counter = u64::MAX - 1;
    advance_program_counter_value(&mut program_counter);
    if program_counter != 2 {
        return Err(ValidationError::CsrDecoder);
    }

    registers[11] = 0xfeed;
    apply_supervisor_call_result(&mut registers, SupervisorCallResult::Legacy { value: 0x12 });
    if registers[10] != 0x12 || registers[11] != 0xfeed {
        return Err(ValidationError::SupervisorCallCompletion);
    }
    apply_supervisor_call_result(
        &mut registers,
        SupervisorCallResult::Modern {
            error: u64::MAX - 1,
            value: 0x34,
        },
    );
    if registers[10] != u64::MAX - 1 || registers[11] != 0x34 {
        return Err(ValidationError::SupervisorCallCompletion);
    }
    let legacy_console = SupervisorCall {
        extension: 1,
        function: 0,
        arguments: [0; 6],
    };
    let modern_timer = SupervisorCall {
        extension: SBI_EXT_TIME,
        function: 0,
        arguments: [0; 6],
    };
    let legacy_clear_ipi = SupervisorCall {
        extension: 3,
        function: 0,
        arguments: [0; 6],
    };
    let mut context = VcpuContext::new(0);
    context.hvip = crate::arch::riscv64::registers::HVIP_GUEST_MASK;
    let clear_ipi_result = emulate_sbi(&mut context, legacy_clear_ipi);
    if !supervisor_completion_matches(legacy_console, SupervisorCallResult::Legacy { value: 0 })
        || supervisor_completion_matches(
            legacy_console,
            SupervisorCallResult::Modern { error: 0, value: 0 },
        )
        || supervisor_timer_matches(legacy_console, SupervisorCallResult::Legacy { value: 0 })
        || !supervisor_timer_matches(
            modern_timer,
            SupervisorCallResult::Modern { error: 0, value: 0 },
        )
        || supervisor_completion_matches(
            modern_timer,
            SupervisorCallResult::Modern { error: 0, value: 0 },
        )
        || modern_extension_available(0x0073_5049)
        || modern_extension_available(0x5246_4e43)
        || !modern_extension_available(SBI_EXT_TIME)
        || context.hvip & (1 << 2) != 0
        || !matches!(
            clear_ipi_result,
            GuestSyncAction::CompleteSupervisorCall(SupervisorCallResult::Legacy {
                value: SBI_SUCCESS
            })
        )
    {
        return Err(ValidationError::SupervisorCallCompletion);
    }
    Ok(())
}

fn validate_illegal_instruction_traps() -> bool {
    let mut context = VcpuContext::new(0);
    let preserved = (2 << 32) | (1 << 18) | (3 << 13);
    context.vstvec = 0x4001;
    context.vsstatus = preserved | (1 << 1) | (1 << 8);
    context.general[5] = 0xfeed;
    // Even a VS-origin read of HSTATUS is outside this guest's CSR contract;
    // decode must reject it so capture forwards an illegal instruction.
    if decode_csr_instruction(0x6000_22f3, &context.general).is_some() {
        return false;
    }
    let instruction = IllegalInstruction {
        program_counter: 0x1234,
        instruction: 0x6000_22f3,
        supervisor: false,
    };
    let action = inject_illegal_instruction(&mut context, instruction);
    if action
        != (GuestSyncAction::EnterSupervisorTrap {
            program_counter: 0x4000,
        })
        || context.vsepc != 0x1234
        || context.vscause != 2
        || context.vstval != 0x6000_22f3
        || context.vsstatus != preserved | (1 << 5)
        || context.supervisor != 1
        || context.general[5] != 0xfeed
    {
        return false;
    }
    // A VS-origin exception saves SPP=1, and a previously disabled SIE clears
    // SPIE even when the old SPIE bit was set. Optional STVAL=0 stays valid.
    context.vsstatus = preserved | (1 << 5);
    context.vstvec = 0x8000;
    let action = inject_illegal_instruction(
        &mut context,
        IllegalInstruction {
            program_counter: 0x5678,
            instruction: 0,
            supervisor: true,
        },
    );
    action
        == (GuestSyncAction::EnterSupervisorTrap {
            program_counter: 0x8000,
        })
        && context.vsstatus == preserved | (1 << 8)
        && context.vsepc == 0x5678
        && context.vstval == 0
        && context.vscause == 2
}
