// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected hardware-virtualization capabilities.
//!
//! Kernel VM policy owns identity, publication, scheduling, demand paging,
//! devices, and exit disposition. This facade owns only the selected vCPU
//! machine state and the mechanisms which operate on it.
//!
//! `VcpuContext`, `Stage2AddressSpace`, and `InterruptController` are selected
//! layout-state re-exports rather than opaque wrappers. Assembly and backend
//! code consume their addresses and layouts directly, so wrapping them would
//! require unchecked pointer conversion or an architecture-to-HAL callback.
//! Kernel policy must still use only the operations exposed by this module.

use hyper::hal::interrupt::{HostInterruptBinding, InterruptId};

/// Native ABI instruction-set identity accepted by the selected backend.
pub const fn guest_architecture_abi() -> u32 {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        hyper::abi::native::HYPER_NATIVE_VIRTUAL_MACHINE_ARCHITECTURE_AARCH64 as u32
    }
    #[cfg(CONFIG_ARCH_RISCV64)]
    {
        hyper::abi::native::HYPER_NATIVE_VIRTUAL_MACHINE_ARCHITECTURE_RISCV64 as u32
    }
    #[cfg(CONFIG_ARCH_X86_64)]
    {
        hyper::abi::native::HYPER_NATIVE_VIRTUAL_MACHINE_ARCHITECTURE_X86_64 as u32
    }
}

/// Reports whether this backend supports the complete userspace-owned VM
/// lifecycle, including administrative stop and acknowledged stage-2
/// retirement.
///
/// Guest entry alone is insufficient: publishing a handle whose last-close
/// path cannot retire its hardware state would make otherwise ordinary
/// process termination fatal. Keep admission closed until every lifecycle
/// operation is implemented for the selected backend.
pub const fn userspace_vm_lifecycle_available() -> bool {
    cfg!(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))
}

/// Assignment requires decoded synchronous MMIO, saved external-interrupt
/// injection and acknowledged VM retirement. Other backends have not qualified
/// that complete combination and must not expose physical device authority.
pub const fn supports_guest_device_assignment() -> bool {
    cfg!(CONFIG_ARCH_AARCH64)
}

const _: () = assert!(
    hyper::abi::native::HYPER_NATIVE_VIRTUAL_MACHINE_ARCHITECTURE_X86_64 <= u32::MAX as u64
);

#[cfg(CONFIG_ARCH_AARCH64)]
pub use crate::arch::vm::GicAccessError;
pub use crate::arch::vm::{
    DeviceError, ExitServiceError, ExitServices, ExitServicesReady, InterruptInitializationError,
    PreparedInterruptVirtualization, RegisterValidationError, Stage2AddressSpace, Stage2Error,
    VcpuContext, VirtualInterruptError,
};
#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub use crate::arch::vm::{GuestSyncAction, GuestSyncExit};
pub use crate::arch::vm::{InterruptController, InterruptError, VcpuInterruptError};

/// Selected virtualization capability which is unavailable on this target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64), allow(dead_code))]
pub enum UnsupportedCapability {
    AdministrativeStop,
    GuestStage2Retirement,
}

/// Pre-mutation proof that this backend supports typed administrative stop.
pub struct AdministrativeStopCapability {
    _private: (),
}

pub fn try_administrative_stop() -> Result<AdministrativeStopCapability, UnsupportedCapability> {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        Ok(AdministrativeStopCapability { _private: () })
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        Err(UnsupportedCapability::AdministrativeStop)
    }
}

/// Opaque local stage-2 invalidation request issued to every sticky CPU.
#[derive(Clone, Copy)]
pub struct GuestStage2RetirementRequest {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    backend: crate::arch::vm::GuestStage2RetirementRequest,
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    never: core::convert::Infallible,
}

/// Pre-mutation proof that the selected backend supports stage-2 retirement.
///
/// Unsupported targets cannot construct this token. Keeping capability
/// discovery separate from request preparation lets callers complete every
/// fallible mechanism check before cutting guest residency or identity state.
pub struct GuestStage2RetirementCapability {
    _private: (),
}

pub fn try_guest_stage2_retirement()
-> Result<GuestStage2RetirementCapability, UnsupportedCapability> {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        Ok(GuestStage2RetirementCapability { _private: () })
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        Err(UnsupportedCapability::GuestStage2Retirement)
    }
}

pub fn prepare_guest_stage2_retirement(
    capability: &GuestStage2RetirementCapability,
    address_space: &Stage2AddressSpace,
) -> GuestStage2RetirementRequest {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        let _ = capability;
        GuestStage2RetirementRequest {
            backend: crate::arch::vm::prepare_guest_stage2_retirement(address_space),
        }
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = (capability, address_space);
        // The private capability cannot be obtained on this target.
        hyper::debug::invariant_failure(
            "selected HAL vm::prepare_guest_stage2_retirement invariant",
        )
    }
}

/// Parks a permanently retired hierarchy and invalidates all local guest tags.
/// All execution owners of the root must have detached before this operation.
pub fn retire_guest_stage2_root_local(root: u64) {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    crate::arch::vm::retire_guest_stage2_root_local(root);
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = root;
        hyper::debug::invariant_failure("guest root retirement unsupported");
    }
}

pub fn service_guest_stage2_live(request: GuestStage2RetirementRequest) {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    crate::arch::vm::service_guest_stage2_live(request.backend);
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    match request.never {}
}

pub fn publish_guest_stage2_changes(_capability: &GuestStage2RetirementCapability) {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    crate::arch::vm::publish_guest_stage2_changes();
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    hyper::debug::invariant_failure("selected HAL vm::publish_guest_stage2_changes invariant");
}

/// Selected per-vCPU machine state retained by one scheduler execution.
pub struct VcpuHardwareState {
    context: VcpuContext,
    runtime_authorized: bool,
}

/// Proof that every dependency required for normal guest entry is active.
///
/// The kernel may mint this only after exit services, register validation,
/// virtual devices, timer routing, and interrupt virtualization have all
/// completed. Copies describe the same irreversible boot publication.
#[derive(Clone, Copy)]
#[must_use]
pub struct VmEntryReady {
    _private: (),
}

impl VcpuHardwareState {
    pub const fn new(context: VcpuContext, _entry: &VmEntryReady) -> Self {
        Self {
            context,
            runtime_authorized: true,
        }
    }

    #[cfg(CONFIG_ARCH_AARCH64)]
    const fn for_validation(context: VcpuContext, _services: &ExitServicesReady) -> Self {
        Self {
            context,
            runtime_authorized: false,
        }
    }
}

pub fn install_exit_services(
    services: ExitServices,
) -> Result<ExitServicesReady, ExitServiceError> {
    crate::arch::vm::install_exit_services(services)
}

/// Commits the one-way transition from callback publication to guest entry.
///
/// # Safety
///
/// Register validation and selected virtual-device initialization must have
/// completed. Every host timer route and interrupt-virtualization dependency
/// must be active and no longer eligible for rollback. The caller must own the
/// only VM initialization transaction and invoke this commit at most once.
pub unsafe fn commit_entry_initialization(_services: ExitServicesReady) -> VmEntryReady {
    VmEntryReady { _private: () }
}

pub fn validate_register_interface() -> Result<(), RegisterValidationError> {
    crate::arch::vm::validate_register_interface()
}

pub fn initialize_devices(
    timer_interrupt: InterruptId,
    host_timer_interrupt: Option<HostInterruptBinding>,
) -> Result<(), DeviceError> {
    crate::arch::vm::initialize_devices(timer_interrupt, host_timer_interrupt)
}

pub fn prepare_interrupts(
    host_timer_interrupt: Option<HostInterruptBinding>,
) -> Result<PreparedInterruptVirtualization, InterruptInitializationError> {
    crate::arch::vm::prepare_interrupts(host_timer_interrupt)
}

pub fn commit_interrupts(
    prepared: PreparedInterruptVirtualization,
) -> Result<(), InterruptInitializationError> {
    crate::arch::vm::commit_interrupts(prepared)
}

pub fn initialize_vcpu_interrupts(
    state: &mut VcpuHardwareState,
) -> Result<(), VirtualInterruptError> {
    state.context.initialize_virtual_interrupts().map(|_| ())
}

/// Sets the guest-visible virtual counter before the context becomes runnable.
pub fn set_virtual_count(context: &mut VcpuContext, physical: u64, value: u64) {
    context.set_virtual_count(physical, value);
}

/// Invalid guest-general-register assignment in an initial context plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitialContextError;

/// One guest-general-register value in an initial machine-context plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InitialRegisterAssignment {
    index: usize,
    value: u64,
}

impl InitialRegisterAssignment {
    pub const fn new(index: usize, value: u64) -> Self {
        Self { index, value }
    }
}

/// Realizes a guest-ABI register plan in the selected machine context.
///
/// Register indices use the selected guest ISA's architectural general-
/// register numbering. Linux boot policy owns the values and their meaning;
/// the HAL owns construction of the machine context which carries them.
pub fn prepare_initial_context(
    entry: u64,
    registers: &[InitialRegisterAssignment],
) -> Result<VcpuContext, InitialContextError> {
    let mut context = VcpuContext::new(entry);
    for assignment in registers {
        let register = context
            .general
            .get_mut(assignment.index)
            .ok_or(InitialContextError)?;
        *register = assignment.value;
    }
    Ok(context)
}

/// Realizes the architecture-neutral Native bootstrap record.
///
/// The four argument slots map to the first four integer argument registers
/// of the selected guest ISA. A nonzero stack is applied where the selected
/// context exposes a boot stack pointer; zero preserves the architectural
/// reset convention.
pub fn prepare_native_bootstrap_context(
    entry: u64,
    stack: u64,
    arguments: [u64; 4],
) -> Result<VcpuContext, InitialContextError> {
    #[cfg(CONFIG_ARCH_RISCV64)]
    const FIRST_ARGUMENT: usize = 10;
    #[cfg(not(CONFIG_ARCH_RISCV64))]
    const FIRST_ARGUMENT: usize = 0;
    let assignments = [
        InitialRegisterAssignment::new(FIRST_ARGUMENT, arguments[0]),
        InitialRegisterAssignment::new(FIRST_ARGUMENT + 1, arguments[1]),
        InitialRegisterAssignment::new(FIRST_ARGUMENT + 2, arguments[2]),
        InitialRegisterAssignment::new(FIRST_ARGUMENT + 3, arguments[3]),
    ];
    let mut context = prepare_initial_context(entry, &assignments)?;
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        context.stack_pointer_el1 = stack;
    }
    #[cfg(CONFIG_ARCH_RISCV64)]
    {
        const SP: usize = 2;
        context.general[SP] = stack;
    }
    #[cfg(CONFIG_ARCH_X86_64)]
    {
        const RSP: usize = 4;
        context.general[RSP] = stack;
    }
    Ok(context)
}

/// Applies the selected architecture's local interrupt state for guest entry.
///
/// Callers must not assume this unmasks interrupts. In particular, `AArch64`
/// keeps IRQs masked across the non-atomic EL2-to-guest context transaction.
pub fn prepare_interrupts_for_entry() {
    crate::arch::vm::prepare_interrupts_for_entry();
}

/// Activates the local machine state for a stopped vCPU.
///
/// # Safety
///
/// `state` must be pinned and exclusively owned by the stopped vCPU. No guest
/// may execute concurrently, and local interrupts must remain masked. A caller
/// which can proceed to guest entry must activate the selected second-stage
/// hierarchy before entry; machine-state-only validation need not do so. An
/// error must leave local vCPU hardware detached so ownership can be released.
pub unsafe fn activate_hardware(
    state: &mut VcpuHardwareState,
    vcpu_id: u32,
    interrupts: &InterruptController,
    physical_count: u64,
) -> Result<bool, VcpuInterruptError> {
    // SAFETY: The facade preserves stopped-state ownership, stage-2, and
    // interrupt-mask requirements while hiding the backend context layout.
    unsafe {
        crate::arch::vm::activate_vcpu_hardware(
            &mut state.context,
            vcpu_id,
            interrupts,
            physical_count,
        )
    }
}

/// Consumes the diagnostic that outer cleanup reused a locally saved bank.
/// Logging belongs to kernel policy, outside the hardware transition.
pub fn take_reused_interrupt_save(state: &mut VcpuHardwareState) -> bool {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        state.context.take_reused_vgic_save()
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = state;
        false
    }
}

/// Saves and detaches the current vCPU's local machine state.
///
/// # Safety
///
/// `state` must exclusively own the active local machine state. Guest execution
/// must have stopped and local interrupts must remain masked.
pub unsafe fn deactivate_hardware(
    state: &mut VcpuHardwareState,
    vcpu_id: u32,
    interrupts: &InterruptController,
    physical_count: u64,
) -> Result<(), VcpuInterruptError> {
    // SAFETY: The facade preserves active ownership and interrupt masking.
    unsafe {
        crate::arch::vm::deactivate_vcpu_hardware(
            &mut state.context,
            vcpu_id,
            interrupts,
            physical_count,
        )
    }
}

/// Opaque exit facts returned only after the selected backend closes guest execution.
///
/// Non-returning backends cannot construct this type: its selected payload is
/// uninhabited. This keeps the lifecycle interface uniform without claiming a
/// typed unwind which the machine backend does not provide.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
pub struct VcpuRunExit {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    backend: crate::arch::vm::GuestRunExit,
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    never: core::convert::Infallible,
}

/// Architecture-neutral terminal policy attributed to valid guest input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
pub enum VcpuTerminalReason {
    MemoryFault,
    Mmio,
    Synchronous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub enum VcpuSynchronousTerminal {
    Undecodable,
    #[cfg(CONFIG_ARCH_AARCH64)]
    Failed {
        exit: GuestSyncExit,
        failure: VcpuInterruptError,
    },
    #[cfg(CONFIG_ARCH_RISCV64)]
    Unsupported(crate::arch::vm::UnsupportedGuestExit),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
pub enum VcpuTerminalCause {
    MemoryFault,
    Mmio,
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    Synchronous(VcpuSynchronousTerminal),
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    Synchronous,
}

/// Complete architecture exit state for one guest-policy terminal stop.
///
/// This copied value remains valid after the selected backend has detached
/// live vCPU hardware. Wait and administrative exits cannot construct it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
pub struct VcpuTerminalExit {
    cause: VcpuTerminalCause,
    syndrome: u64,
    fault_address: u64,
    program_counter: u64,
    processor_state: u64,
    vector: u64,
}

impl VcpuTerminalExit {
    pub const fn reason(self) -> VcpuTerminalReason {
        match self.cause {
            VcpuTerminalCause::MemoryFault => VcpuTerminalReason::MemoryFault,
            VcpuTerminalCause::Mmio => VcpuTerminalReason::Mmio,
            #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
            VcpuTerminalCause::Synchronous(_) => VcpuTerminalReason::Synchronous,
            #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
            VcpuTerminalCause::Synchronous => VcpuTerminalReason::Synchronous,
        }
    }

    pub const fn cause(self) -> VcpuTerminalCause {
        self.cause
    }

    pub const fn syndrome(self) -> u64 {
        self.syndrome
    }

    pub const fn fault_address(self) -> u64 {
        self.fault_address
    }

    pub const fn program_counter(self) -> u64 {
        self.program_counter
    }

    pub const fn processor_state(self) -> u64 {
        self.processor_state
    }

    pub const fn vector(self) -> u64 {
        self.vector
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
pub enum VcpuAdministrativeStopReason {
    Requested,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
pub enum VcpuWaitReason {
    Interrupt,
    Device,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
pub enum VcpuTimerWake {
    None,
    PendingNow,
    Deadline(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
pub struct VcpuWfiState {
    pub interrupt_may_wake: bool,
    pub timer: VcpuTimerWake,
}

pub fn stopped_wfi_state(
    state: &VcpuHardwareState,
    vcpu_id: u32,
    interrupts: &InterruptController,
    physical_count: u64,
) -> Result<VcpuWfiState, StoppedVcpuQueryError> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        let interrupt_may_wake = interrupts
            .may_wake_wfi(hyper::vm::interrupt::VirtualCpuId::new(vcpu_id))
            .map_err(crate::arch::vm::VcpuInterruptError::Controller)
            .map_err(StoppedVcpuQueryError::Backend)?;
        let timer = match state.context.virtual_timer_wfi_wake_at(physical_count) {
            hyper::drivers::timer::arm_generic::VirtualTimerWake::None => VcpuTimerWake::None,
            hyper::drivers::timer::arm_generic::VirtualTimerWake::PendingNow => {
                VcpuTimerWake::PendingNow
            }
            hyper::drivers::timer::arm_generic::VirtualTimerWake::Deadline(deadline) => {
                VcpuTimerWake::Deadline(deadline)
            }
        };
        Ok(VcpuWfiState {
            interrupt_may_wake,
            timer,
        })
    }
    #[cfg(CONFIG_ARCH_RISCV64)]
    {
        let stopped = crate::arch::vm::stopped_guest_wfi_state(
            &state.context,
            vcpu_id,
            interrupts,
            physical_count,
        )
        .map_err(StoppedVcpuQueryError::Backend)?;
        let timer = match stopped.timer {
            hyper::vm::riscv64::time::TimerWake::Disabled => VcpuTimerWake::None,
            hyper::vm::riscv64::time::TimerWake::PendingNow => VcpuTimerWake::PendingNow,
            hyper::vm::riscv64::time::TimerWake::AfterTicks(ticks) => {
                VcpuTimerWake::Deadline(physical_count.wrapping_add(ticks))
            }
        };
        Ok(VcpuWfiState {
            interrupt_may_wake: stopped.interrupt_may_wake,
            timer,
        })
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = (state, vcpu_id, interrupts, physical_count);
        Err(StoppedVcpuQueryError::Unsupported)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub enum StoppedVcpuQueryError {
    Unsupported,
    Backend(VcpuInterruptError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveInterruptReconcileError {
    #[cfg_attr(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64), allow(dead_code))]
    Unsupported,
    #[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
    Backend(VcpuInterruptError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
pub enum VcpuRunDisposition {
    Wait(VcpuWaitReason),
    Terminal(VcpuTerminalExit),
    AdministrativeStop(VcpuAdministrativeStopReason),
}

impl core::fmt::Display for VcpuTerminalReason {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::MemoryFault => "memory fault policy stop",
            Self::Mmio => "MMIO policy stop",
            Self::Synchronous => "synchronous exit policy stop",
        })
    }
}

impl VcpuRunExit {
    #[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
    pub const fn disposition(self) -> VcpuRunDisposition {
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        {
            match self.backend {
                #[cfg(CONFIG_ARCH_AARCH64)]
                crate::arch::vm::GuestRunExit::Wait(crate::arch::vm::GuestWaitReason::Device) => {
                    VcpuRunDisposition::Wait(VcpuWaitReason::Device)
                }
                crate::arch::vm::GuestRunExit::Wait(
                    crate::arch::vm::GuestWaitReason::Interrupt,
                ) => VcpuRunDisposition::Wait(VcpuWaitReason::Interrupt),
                crate::arch::vm::GuestRunExit::Terminal(exit) => {
                    let cause = match exit.cause() {
                        crate::arch::vm::GuestTerminalCause::MemoryFault => {
                            VcpuTerminalCause::MemoryFault
                        }
                        crate::arch::vm::GuestTerminalCause::Mmio => VcpuTerminalCause::Mmio,
                        crate::arch::vm::GuestTerminalCause::Synchronous(synchronous) => {
                            let synchronous = match synchronous {
                                crate::arch::vm::GuestSynchronousTerminal::Undecodable => {
                                    VcpuSynchronousTerminal::Undecodable
                                }
                                #[cfg(CONFIG_ARCH_AARCH64)]
                                crate::arch::vm::GuestSynchronousTerminal::Failed {
                                    exit,
                                    failure:
                                        crate::arch::vm::GuestSyncFailure::VirtualInterrupt(failure),
                                } => VcpuSynchronousTerminal::Failed { exit, failure },
                                #[cfg(CONFIG_ARCH_RISCV64)]
                                crate::arch::vm::GuestSynchronousTerminal::Unsupported(exit) => {
                                    VcpuSynchronousTerminal::Unsupported(exit)
                                }
                            };
                            VcpuTerminalCause::Synchronous(synchronous)
                        }
                    };
                    VcpuRunDisposition::Terminal(VcpuTerminalExit {
                        cause,
                        syndrome: exit.syndrome(),
                        fault_address: exit.fault_address(),
                        program_counter: exit.program_counter(),
                        processor_state: exit.processor_state(),
                        vector: exit.vector(),
                    })
                }
                crate::arch::vm::GuestRunExit::AdministrativeStop(
                    crate::arch::vm::GuestAdministrativeStopReason::Requested,
                ) => {
                    VcpuRunDisposition::AdministrativeStop(VcpuAdministrativeStopReason::Requested)
                }
            }
        }
        #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
        {
            match self.never {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
pub enum VcpuRunError {
    Owner,
    Return,
    State,
}

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
impl From<crate::arch::vm::GuestRunError> for VcpuRunError {
    fn from(error: crate::arch::vm::GuestRunError) -> Self {
        match error {
            crate::arch::vm::GuestRunError::Owner => Self::Owner,
            crate::arch::vm::GuestRunError::Return => Self::Return,
            crate::arch::vm::GuestRunError::State => Self::State,
        }
    }
}

/// Linear proof that the selected backend has stopped lower-world execution.
///
/// On non-returning backends the payload is uninhabited, so safe code cannot
/// manufacture a false stopped-state proof.
#[must_use = "stopped vCPU hardware must be detached exactly once"]
#[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
pub struct StoppedVcpuRun {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    backend: crate::arch::vm::StoppedGuestRun,
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    never: core::convert::Infallible,
}

impl StoppedVcpuRun {
    #[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
    pub fn exit(&self) -> VcpuRunExit {
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        {
            VcpuRunExit {
                backend: self.backend.exit(),
            }
        }
        #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
        {
            match self.never {}
        }
    }
}

/// Runs an active vCPU until the selected backend either returns a linear
/// stopped proof or transfers control through its non-returning entry path.
///
/// # Safety
///
/// `state` must point to initialized, pinned, exclusively owned hardware state
/// for the entire run. Its guest translation and virtual hardware must be
/// active on the current CPU, with local interrupt masking satisfying the
/// selected entry contract. No concurrent caller may access the context.
#[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
pub unsafe fn run(state: *mut VcpuHardwareState) -> Result<StoppedVcpuRun, VcpuRunError> {
    if state.is_null() || !state.is_aligned() {
        return Err(VcpuRunError::Owner);
    }
    // SAFETY: The caller guarantees a valid exclusive state pointer.
    if !unsafe { (*state).runtime_authorized } {
        hyper::debug::invariant_failure("selected HAL vm::run invariant")
    }
    // SAFETY: The validated state pointer exclusively owns this pinned field.
    let context = unsafe { core::ptr::addr_of_mut!((*state).context) };
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        // SAFETY: The facade preserves the backend's active, pinned run contract.
        unsafe { VcpuContext::run(context) }
            .map(|backend| StoppedVcpuRun { backend })
            .map_err(Into::into)
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        // SAFETY: The facade preserves the backend's non-returning entry contract.
        unsafe { VcpuContext::enter(context) }
    }
}

pub struct StoppedDetachFailure {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    backend: crate::arch::vm::StoppedDeactivationFailure,
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    never: core::convert::Infallible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub enum StoppedDetachError {
    Unsupported,
    Backend(VcpuInterruptError),
}

impl StoppedDetachFailure {
    pub const fn error(&self) -> StoppedDetachError {
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        {
            StoppedDetachError::Backend(self.backend.error())
        }
        #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
        {
            match self.never {}
        }
    }
}

/// Detaches hardware for a stopped run whose lower world is closed.
///
/// # Safety
///
/// `stopped` must come from the last run of this exact pinned `state`, on the
/// current CPU. The vCPU identity and interrupt controller must match that
/// activation. Local interrupts must remain masked and no guest may execute
/// concurrently until detachment completes.
pub unsafe fn deactivate_stopped_hardware(
    state: &mut VcpuHardwareState,
    vcpu_id: u32,
    interrupts: &InterruptController,
    physical_count: u64,
    stopped: StoppedVcpuRun,
) -> Result<(), StoppedDetachFailure> {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        // SAFETY: The caller preserves the stopped proof, exact context, and mask.
        unsafe {
            crate::arch::vm::deactivate_stopped_vcpu_hardware(
                &mut state.context,
                vcpu_id,
                interrupts,
                physical_count,
                stopped.backend,
            )
        }
        .map_err(|backend| StoppedDetachFailure { backend })
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = (state, vcpu_id, interrupts, physical_count);
        match stopped.never {}
    }
}

pub fn handle_virtual_timer_interrupt(
    state: &mut VcpuHardwareState,
    vcpu_id: u32,
    interrupts: &InterruptController,
) -> Result<bool, VcpuInterruptError> {
    crate::arch::vm::handle_virtual_timer_interrupt(&mut state.context, vcpu_id, interrupts)
}

pub fn handle_maintenance_interrupt(
    state: &mut VcpuHardwareState,
    vcpu_id: u32,
    interrupts: &InterruptController,
) -> Result<bool, VcpuInterruptError> {
    crate::arch::vm::handle_maintenance_interrupt(&mut state.context, vcpu_id, interrupts)
}

pub fn maintenance_interrupt_pending() -> bool {
    crate::arch::vm::maintenance_interrupt_pending()
}

/// Disables local virtual-interrupt delivery after ownership is lost.
pub fn quiesce_virtual_interrupt_delivery() {
    crate::arch::vm::quiesce_virtual_interrupt_delivery();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptVirtualizationDescription {
    pub list_registers: u8,
    pub priority_bits: u8,
    pub preemption_bits: u8,
    pub interrupt_id_bits: u8,
}

pub fn interrupt_virtualization_description() -> Option<InterruptVirtualizationDescription> {
    crate::arch::vm::interrupt_virtualization_description().map(
        |(list_registers, priority_bits, preemption_bits, interrupt_id_bits)| {
            InterruptVirtualizationDescription {
                list_registers,
                priority_bits,
                preemption_bits,
                interrupt_id_bits,
            }
        },
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerValidationError {
    #[cfg(CONFIG_ARCH_AARCH64)]
    InterruptController(InterruptError),
    #[cfg(CONFIG_ARCH_AARCH64)]
    InvalidInterrupt,
    #[cfg(CONFIG_ARCH_AARCH64)]
    VirtualInterrupt(VirtualInterruptError),
}

/// Prepared admission to the selected backend's destructive timer probe.
///
/// Unsupported targets return `None` and cannot construct the private proof
/// required by injection or result inspection.
pub struct PreparedTimerValidation {
    capability: TimerValidationCapability,
    interrupts: InterruptController,
    hardware: VcpuHardwareState,
}

pub struct TimerValidationCapability {
    #[cfg(CONFIG_ARCH_AARCH64)]
    _private: (),
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    never: core::convert::Infallible,
}

impl PreparedTimerValidation {
    pub fn into_parts(
        self,
    ) -> (
        TimerValidationCapability,
        InterruptController,
        VcpuHardwareState,
    ) {
        (self.capability, self.interrupts, self.hardware)
    }
}

pub fn prepare_timer_validation(
    timer_interrupt: InterruptId,
    physical_count: u64,
    services: &ExitServicesReady,
    prepared: &PreparedInterruptVirtualization,
) -> Result<Option<PreparedTimerValidation>, TimerValidationError> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        let timer = hyper::vm::arm::gic::GicInterruptId::new(timer_interrupt.get())
            .ok_or(TimerValidationError::InvalidInterrupt)?;
        let interrupts = InterruptController::new(1, timer, usize::from(prepared.list_registers()))
            .map_err(TimerValidationError::InterruptController)?;
        interrupts.enable_distributor_for_validation();
        let mut context = VcpuContext::new(0);
        context
            .initialize_virtual_interrupts()
            .map_err(TimerValidationError::VirtualInterrupt)?;
        context.set_virtual_count(physical_count, physical_count);
        context.set_virtual_timer_deadline(physical_count.wrapping_add(1_000_000));
        context.set_virtual_timer_enabled(true);
        Ok(Some(PreparedTimerValidation {
            capability: TimerValidationCapability { _private: () },
            interrupts,
            hardware: VcpuHardwareState::for_validation(context, services),
        }))
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = (timer_interrupt, physical_count, services, prepared);
        Ok(None)
    }
}

/// Allocation-free construction plan for one bounded virtual interrupt model.
///
/// Kernel policy admits `allocation_size` before consuming this plan. The
/// realized controller verifies that its retained layout matches the same
/// architecture-owned contract before it can be published.
pub struct PreparedInterruptController {
    vcpu_count: u32,
    timer_interrupt: hyper::vm::interrupt::VirtualInterruptId,
    #[cfg(CONFIG_ARCH_AARCH64)]
    list_registers: usize,
    allocation_size: usize,
}

pub fn prepare_interrupt_controller(
    vcpu_count: u32,
    timer_interrupt: hyper::vm::interrupt::VirtualInterruptId,
) -> Result<PreparedInterruptController, InterruptError> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        let _timer = hyper::vm::arm::gic::GicInterruptId::new(timer_interrupt.get())
            .ok_or(InterruptError::InvalidInterrupt)?;
        let description =
            interrupt_virtualization_description().ok_or(InterruptError::MissingCapabilities)?;
        let list_registers = usize::from(description.list_registers);
        let allocation_size =
            InterruptController::allocation_requirement(vcpu_count, list_registers)?;
        Ok(PreparedInterruptController {
            vcpu_count,
            timer_interrupt,
            list_registers,
            allocation_size,
        })
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        Ok(PreparedInterruptController {
            vcpu_count,
            timer_interrupt,
            allocation_size: 0,
        })
    }
}

pub const fn prepared_interrupt_controller_allocation_size(
    prepared: &PreparedInterruptController,
) -> usize {
    prepared.allocation_size
}

pub fn create_prepared_interrupt_controller(
    prepared: PreparedInterruptController,
) -> Result<InterruptController, InterruptError> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        let timer = hyper::vm::arm::gic::GicInterruptId::new(prepared.timer_interrupt.get())
            .ok_or(InterruptError::InvalidInterrupt)?;
        let controller =
            InterruptController::new(prepared.vcpu_count, timer, prepared.list_registers)?;
        if interrupt_controller_allocation_size(&controller) != Some(prepared.allocation_size) {
            return Err(InterruptError::Build(
                hyper::vm::arm::gic::BuildError::InvalidStoragePlan,
            ));
        }
        Ok(controller)
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        InterruptController::new(prepared.vcpu_count, prepared.timer_interrupt)
    }
}

#[cfg(CONFIG_ARCH_AARCH64)]
pub fn interrupt_controller_allocation_size(controller: &InterruptController) -> Option<usize> {
    controller.allocation_size()
}

pub fn inject_timer_for_validation(
    capability: &TimerValidationCapability,
    state: &mut VcpuHardwareState,
    vcpu_id: u32,
    interrupts: &InterruptController,
) -> Result<(), VcpuInterruptError> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        let _ = capability;
        crate::arch::vm::inject_timer_for_validation(&mut state.context, vcpu_id, interrupts)
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = (state, vcpu_id, interrupts);
        match capability.never {}
    }
}

pub fn timer_validation_succeeded(
    capability: &TimerValidationCapability,
    interrupts: &InterruptController,
) -> Result<bool, InterruptError> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        let _ = capability;
        let snapshot = interrupts
            .timer_snapshot(hyper::vm::interrupt::VirtualCpuId::new(0))
            .map_err(InterruptError::Vgic)?;
        Ok(snapshot.pending && snapshot.listed)
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = interrupts;
        match capability.never {}
    }
}

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub fn handle_guest_sync(
    state: &mut VcpuHardwareState,
    vcpu_id: u32,
    interrupts: &InterruptController,
    exit: GuestSyncExit,
) -> GuestSyncAction {
    crate::arch::vm::handle_guest_sync(&mut state.context, vcpu_id, interrupts, exit)
}

#[cfg(CONFIG_ARCH_AARCH64)]
pub fn update_guest_device_interrupt(
    state: &mut VcpuHardwareState,
    vcpu_id: u32,
    interrupts: &InterruptController,
    interrupt: hyper::vm::arm::gic::GicInterruptId,
    asserted: bool,
) -> Result<(), VcpuInterruptError> {
    crate::arch::vm::update_guest_device_interrupt(
        &mut state.context,
        vcpu_id,
        interrupts,
        interrupt,
        asserted,
    )
}

#[cfg(CONFIG_ARCH_AARCH64)]
pub fn update_saved_guest_device_interrupt(
    interrupts: &InterruptController,
    vcpu_id: u32,
    interrupt: hyper::vm::arm::gic::GicInterruptId,
    asserted: bool,
) -> Result<(), VcpuInterruptError> {
    crate::arch::vm::update_saved_guest_device_interrupt(interrupts, vcpu_id, interrupt, asserted)
}

pub fn reconcile_active_interrupts(
    state: &mut VcpuHardwareState,
    vcpu_id: u32,
    interrupts: &InterruptController,
) -> Result<(), ActiveInterruptReconcileError> {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        crate::arch::vm::reconcile_active_interrupts(&mut state.context, vcpu_id, interrupts)
            .map_err(ActiveInterruptReconcileError::Backend)
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = (state, vcpu_id, interrupts);
        Err(ActiveInterruptReconcileError::Unsupported)
    }
}

/// Prompts the selected CPU to leave guest execution without publishing
/// scheduler policy. `false` explicitly reports that the selected backend has
/// no qualified targeted guest-exit mechanism; it does not consume the
/// caller's durable stop request.
pub fn request_guest_exit(cpu: hyper::cpu::CpuIndex) -> bool {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        crate::arch::vm::request_guest_exit(cpu)
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = cpu;
        false
    }
}

#[cfg(CONFIG_ARCH_AARCH64)]
pub fn access_guest_gic(
    state: &mut VcpuHardwareState,
    vcpu_id: u32,
    interrupts: &InterruptController,
    access: hyper::vm::arm::gic::mmio::DecodedAccess,
    operation: hyper::vm::exit::MmioOperation,
) -> Result<Option<u64>, GicAccessError> {
    crate::arch::vm::access_guest_gic(&mut state.context, vcpu_id, interrupts, access, operation)
}

/// Whether activation was deferred before loading any guest hardware.
pub fn interrupt_entry_deferred(error: &VcpuInterruptError) -> bool {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        matches!(error, VcpuInterruptError::InterruptGateClosed)
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = error;
        false
    }
}

pub fn interrupt_entry_gate_closed(interrupts: &InterruptController) -> bool {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        interrupts.entry_gate_closed()
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = interrupts;
        false
    }
}

pub fn interrupt_access_pending(interrupts: &InterruptController, vcpu: u32) -> bool {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        interrupts.active_access_pending(vcpu)
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = (interrupts, vcpu);
        false
    }
}

/// Completes an internal GIC transaction without a userspace device request.
pub fn take_interrupt_access(
    interrupts: &InterruptController,
    vcpu: u32,
) -> Option<hyper::vm::exit::MmioAction> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        interrupts
            .take_active_access(vcpu)
            .map(|result| match result {
                Ok(Some(value)) => hyper::vm::exit::MmioAction::CompleteRead(value),
                Ok(None) => hyper::vm::exit::MmioAction::CompleteWrite,
                Err(_) => hyper::vm::exit::MmioAction::Stop,
            })
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = (interrupts, vcpu);
        None
    }
}

pub fn cancel_interrupt_access(interrupts: &InterruptController, vcpu: u32) {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        interrupts.cancel_active_access(vcpu);
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = (interrupts, vcpu);
    }
}

#[cfg(feature = "kernel-self-test")]
pub fn guest_execution_available() -> bool {
    crate::arch::vm::guest_execution_available()
}

/// Logical identifier width agreed by all admitted CPUs before VM reservation.
pub fn guest_translation_identifier_bits() -> Result<u8, Stage2Error> {
    crate::arch::vm::guest_translation_identifier_bits()
}

#[cfg(CONFIG_ARCH_RISCV64)]
pub fn update_guest_device_interrupt(
    state: &mut VcpuHardwareState,
    vcpu_id: u32,
    interrupts: &InterruptController,
    interrupt: hyper::vm::interrupt::VirtualInterruptId,
    asserted: bool,
) -> Result<(), VcpuInterruptError> {
    crate::arch::vm::update_guest_device_interrupt(
        &mut state.context,
        vcpu_id,
        interrupts,
        interrupt,
        asserted,
    )
}

#[cfg(CONFIG_ARCH_RISCV64)]
pub fn update_saved_guest_device_interrupt(
    interrupts: &InterruptController,
    vcpu_id: u32,
    interrupt: hyper::vm::interrupt::VirtualInterruptId,
    asserted: bool,
) -> Result<(), VcpuInterruptError> {
    crate::arch::vm::update_saved_guest_device_interrupt(interrupts, vcpu_id, interrupt, asserted)
}

#[cfg(CONFIG_ARCH_RISCV64)]
pub fn access_plic(
    state: &mut VcpuHardwareState,
    interrupts: &InterruptController,
    vcpu_id: u32,
    offset: u64,
    size: usize,
    operation: hyper::vm::exit::MmioOperation,
) -> Result<Option<u64>, VcpuInterruptError> {
    crate::arch::vm::access_plic(
        &mut state.context,
        interrupts,
        vcpu_id,
        offset,
        size,
        operation,
    )
}

/// A cached mapping epoch may be reused only while its hardware root is selected.
pub fn stage2_selection_is_current(address_space: &Stage2AddressSpace) -> bool {
    crate::arch::vm::stage2_selection_is_current(address_space)
}

/// All admitted CPUs satisfy the RV64 I/M/A/F/D/C, Zicsr, Zifencei and Sstc
/// baseline. This is a mechanism guarantee, not an enumeration of host extras.
pub fn riscv_guest_baseline_available() -> bool {
    #[cfg(CONFIG_ARCH_RISCV64)]
    {
        crate::arch::vm::riscv_guest_baseline_available()
    }
    #[cfg(not(CONFIG_ARCH_RISCV64))]
    {
        false
    }
}

/// Availability of the selected platform's implemented VM backend.
pub fn platform_supported() -> bool {
    crate::arch::vm::platform_supported()
}

pub fn guest_gic_version() -> u32 {
    crate::arch::vm::guest_gic_version()
}

/// Returns the vCPUs whose interrupt models changed since the last drain.
pub fn take_reconcile_targets(interrupts: &InterruptController) -> u64 {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        interrupts.take_reconcile_targets()
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = interrupts;
        0
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum PowerContextError {
    Unsupported,
    Interrupt(VirtualInterruptError),
    Controller(hyper::vm::arm::gic::RuntimeError),
}

pub fn reset_vcpu_interrupts(
    interrupts: &InterruptController,
    vcpu: u32,
) -> Result<(), PowerContextError> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        interrupts
            .reset_vcpu(hyper::vm::interrupt::VirtualCpuId::new(vcpu))
            .map_err(PowerContextError::Controller)
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = (interrupts, vcpu);
        Err(PowerContextError::Unsupported)
    }
}

/// Reinitializes a hardware-detached vCPU before its next power-on entry.
pub fn reset_power_context(
    state: &mut VcpuHardwareState,
    entry: u64,
    argument: u64,
) -> Result<(), PowerContextError> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        let mut context = VcpuContext::new(entry);
        context.general[0] = argument;
        context
            .initialize_virtual_interrupts()
            .map_err(PowerContextError::Interrupt)?;
        state.context = context;
        Ok(())
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = (state, entry, argument);
        Err(PowerContextError::Unsupported)
    }
}

pub fn complete_power_call(
    state: &mut VcpuHardwareState,
    result: i64,
) -> Result<(), PowerContextError> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        state.context.general[0] = hyper::vm::arm::psci::return_register(result);
        Ok(())
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = (state, result);
        Err(PowerContextError::Unsupported)
    }
}

pub const fn maximum_guest_vcpus() -> u32 {
    crate::arch::vm::maximum_guest_vcpus()
}

#[derive(Debug)]
pub enum DeviceContextError {
    #[cfg_attr(CONFIG_ARCH_AARCH64, allow(dead_code))]
    Unsupported,
    #[cfg_attr(not(CONFIG_ARCH_AARCH64), allow(dead_code))]
    InvalidState,
}

pub fn complete_device_call(
    state: &mut VcpuHardwareState,
    action: hyper::vm::exit::MmioAction,
) -> Result<(), DeviceContextError> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        state
            .context
            .complete_device(action)
            .map_err(|_| DeviceContextError::InvalidState)
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = (state, action);
        Err(DeviceContextError::Unsupported)
    }
}

/// A saved guest device line could not be updated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceLineError {
    InvalidInterrupt,
    Backend,
    Unsupported,
}

/// Sets a validated shared guest device line without touching a live vCPU bank.
/// Unsupported architectures reject notification-device admission.
pub fn update_saved_device_line(
    interrupts: &InterruptController,
    interrupt: u32,
    asserted: bool,
) -> Result<(), DeviceLineError> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        let interrupt = hyper::vm::arm::gic::GicInterruptId::new(interrupt)
            .ok_or(DeviceLineError::InvalidInterrupt)?;
        update_saved_guest_device_interrupt(interrupts, 0, interrupt, asserted)
            .map_err(|_| DeviceLineError::Backend)
    }
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    {
        let _ = (interrupts, interrupt, asserted);
        Err(DeviceLineError::Unsupported)
    }
}

/// Selected backend has saved shared-IRQ state and guest MMIO dispatch needed
/// by VM-owned mailboxes and direct notification bindings.
pub const fn supports_io_notifications() -> bool {
    cfg!(CONFIG_ARCH_AARCH64)
}

/// Completes local invalidation of every guest translation identifier.
/// The caller keeps guest execution stopped and local interrupts masked.
pub fn invalidate_guest_translation_namespace_local() {
    crate::arch::vm::invalidate_guest_translation_namespace_local();
}
