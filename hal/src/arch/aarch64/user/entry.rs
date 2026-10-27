// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native EL0 exception return and architecture-private unwind ownership.
//!
//! A syscall retains its private vector frame on the owning Thread's stack
//! across blocking and migration. Policy receives only owned invocation values.
//! Actual switches save the lower register bank and withdraw CPU publication;
//! faults, lower-EL preemption, and stop copy into the persistent context.

use core::arch::asm;
use core::mem::{offset_of, size_of};
use core::ptr::NonNull;

use hyper::abi::native::NativeInvocation;
use hyper::hal::user::{NativeCallAction, NativeCallService, UserFault, UserRunBinding};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
use hyper::sync::atomic::AtomicUsize;
use hyper::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::arch::aarch64::exception::ExceptionFrame;
use {crate::arch::aarch64::registers, crate::arch::aarch64::user};
use {
    crate::arch::aarch64::user::contract::LowerElReturnRegime,
    crate::arch::aarch64::user::contract::UserMachineContractError,
};

const MAX_CPUS: usize = hyper::config::MAX_CPUS as usize;

#[repr(C, align(16))]
struct MachineContext {
    general: [u64; 31],
    program_counter: u64,
    processor_state: u64,
    stack_pointer: u64,
    fp: crate::arch::aarch64::fp::State,
    tpidr_el0: u64,
    tpidrro_el0: u64,
    thread: u64,
    image_generation: u64,
    run_generation: u64,
    state: u64,
    exit_kind: u64,
    syndrome: u64,
    fault_address: u64,
    entry_hcr: u64,
}

const _: () = {
    assert!(offset_of!(MachineContext, general) == registers::USER_CONTEXT_X0_OFFSET as usize);
    assert!(
        offset_of!(MachineContext, program_counter) == registers::USER_CONTEXT_PC_OFFSET as usize
    );
    assert!(
        offset_of!(MachineContext, processor_state)
            == registers::USER_CONTEXT_PSTATE_OFFSET as usize
    );
    assert!(
        offset_of!(MachineContext, stack_pointer) == registers::USER_CONTEXT_SP_OFFSET as usize
    );
    assert!(offset_of!(MachineContext, fp) == registers::USER_CONTEXT_FP_OFFSET as usize);
    assert!(
        offset_of!(MachineContext, tpidr_el0) == registers::USER_CONTEXT_TPIDR_EL0_OFFSET as usize
    );
    assert!(
        offset_of!(MachineContext, tpidrro_el0)
            == registers::USER_CONTEXT_TPIDRRO_EL0_OFFSET as usize
    );
    assert!(offset_of!(MachineContext, thread) == registers::USER_CONTEXT_THREAD_OFFSET as usize);
    assert!(
        offset_of!(MachineContext, image_generation)
            == registers::USER_CONTEXT_IMAGE_GENERATION_OFFSET as usize
    );
    assert!(
        offset_of!(MachineContext, run_generation)
            == registers::USER_CONTEXT_RUN_GENERATION_OFFSET as usize
    );
    assert!(offset_of!(MachineContext, state) == registers::USER_CONTEXT_STATE_OFFSET as usize);
    assert!(
        offset_of!(MachineContext, exit_kind) == registers::USER_CONTEXT_EXIT_KIND_OFFSET as usize
    );
    assert!(
        offset_of!(MachineContext, syndrome) == registers::USER_CONTEXT_SYNDROME_OFFSET as usize
    );
    assert!(
        offset_of!(MachineContext, fault_address)
            == registers::USER_CONTEXT_FAULT_ADDRESS_OFFSET as usize
    );
    assert!(
        offset_of!(MachineContext, entry_hcr) == registers::USER_CONTEXT_ENTRY_HCR_OFFSET as usize
    );
    assert!(size_of::<MachineContext>() == registers::USER_CONTEXT_SIZE as usize);
};

/// Pinned architecture register owner attached to one scheduler `UserThread`.
pub(crate) struct UserContext {
    machine: MachineContext,
}

impl UserContext {
    pub(crate) fn snapshot_registers(&self, output: &mut hyper::hal::user::UserRegisterSnapshot) {
        output.machine = 183;
        output.general[..31].copy_from_slice(&self.machine.general);
        output.pc = self.machine.program_counter;
        output.sp = self.machine.stack_pointer;
        output.tls = self.machine.tpidr_el0;
        output.processor_state = self.machine.processor_state;
        self.machine.fp.snapshot(output);
    }

    pub(crate) fn set_entry_argument(&mut self, argument: u64) {
        self.machine.general[0] = argument;
    }

    pub(crate) fn try_new(
        entry: u64,
        stack: u64,
        tls: u64,
        address_limit: u64,
    ) -> Result<Self, Error> {
        if entry == 0
            || entry >= address_limit
            || !entry.is_multiple_of(registers::AARCH64_INSTRUCTION_SIZE)
            || stack == 0
            || stack >= address_limit
            || !stack.is_multiple_of(16)
            || tls >= address_limit
        {
            return Err(Error::InvalidInitialContext);
        }
        user::execution_capabilities()?;
        Ok(Self {
            machine: MachineContext {
                general: [0; 31],
                program_counter: entry,
                // Native userspace currently owns normal IRQ delivery only.
                // Keep debug, SError, and FIQ masked until those classes have
                // explicit context ownership and contained return paths.
                processor_state: native_processor_state(),
                stack_pointer: stack,
                fp: crate::arch::aarch64::fp::State::zeroed(),
                tpidr_el0: tls,
                tpidrro_el0: 0,
                thread: 0,
                image_generation: 0,
                run_generation: 0,
                state: registers::USER_CONTEXT_STATE_READY,
                exit_kind: registers::USER_CONTEXT_EXIT_NONE,
                syndrome: 0,
                fault_address: 0,
                entry_hcr: 0,
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AlreadyRunning,
    CompletionBindingMismatch,
    InvalidInitialContext,
    InvalidMachineState,
    InvalidProcessor,
    LowerWorldUnavailable,
    RunGenerationNotIncreasing,
    Unsupported(UserMachineContractError),
}

impl From<UserMachineContractError> for Error {
    fn from(error: UserMachineContractError) -> Self {
        Self::Unsupported(error)
    }
}

struct RunPublication {
    /// A nonzero generation publishes the preceding context and service.
    generation: AtomicU64,
    context: AtomicPtr<MachineContext>,
    service: AtomicPtr<NativeCallService<'static>>,
    syscall_frame: AtomicPtr<ExceptionFrame>,
}

impl RunPublication {
    const fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            context: AtomicPtr::new(core::ptr::null_mut()),
            service: AtomicPtr::new(core::ptr::null_mut()),
            syscall_frame: AtomicPtr::new(core::ptr::null_mut()),
        }
    }
}

static ACTIVE_RUNS: [RunPublication; MAX_CPUS] = [const { RunPublication::new() }; MAX_CPUS];
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
static DIRECT_NATIVE_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
static MACHINE_RUNS: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
static SYSCALL_SUSPENSIONS: AtomicUsize = AtomicUsize::new(0);

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn native_run_counts_for_test() -> (usize, usize) {
    (
        MACHINE_RUNS.load(Ordering::Relaxed),
        SYSCALL_SUSPENSIONS.load(Ordering::Relaxed),
    )
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn syscall_fp_resident_for_test() -> bool {
    let Ok(publication) = local_publication() else {
        return false;
    };
    if publication.generation.load(Ordering::Acquire) == 0 {
        return false;
    }
    let Some(frame) = NonNull::new(publication.syscall_frame.load(Ordering::Relaxed)) else {
        return false;
    };
    // SAFETY: The test caller masks IRQs while its current syscall owns this
    // original frame. No frame reference is used by the suspended dispatcher.
    unsafe { frame.as_ref().return_cptr & registers::CPTR_EL2_FPEN_ENABLED != 0 }
}

unsafe extern "C" {
    fn aarch64_run_native_user(context: *mut MachineContext);
    fn aarch64_unwind_native_user();
    fn aarch64_suspend_native_registers(frame: *const ExceptionFrame);
    fn aarch64_resume_native_registers(
        context: *const MachineContext,
        frame: *const ExceptionFrame,
    );
}

/// Runs an admitted Thread-owned generation through sleepable syscall entries.
///
/// # Safety
/// The caller retains context and service at stable addresses until this run
/// closes. Initial entry and final return are IRQ-masked. Scheduler switches
/// suspend this CPU's Native publication and translation; syscall return first
/// restores an admitted translation on the current CPU. No CPU-affine guard
/// may span the call.
pub(crate) unsafe fn run_user<'context>(
    context: &'context mut UserContext,
    binding: UserRunBinding,
    service: &NativeCallService<'_>,
) -> Result<UserExit<'context>, Error> {
    if !crate::arch::aarch64::lower_el::native_world_available() {
        return Err(Error::LowerWorldUnavailable);
    }
    prepare_run(context, binding)?;
    let active = ActiveRun {
        context: NonNull::from(&mut context.machine),
        generation: binding.run_generation(),
        service: NonNull::from(service).cast(),
    };
    if let Err(error) = publish(active) {
        context.machine.state = registers::USER_CONTEXT_STATE_READY;
        return Err(error);
    }
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    MACHINE_RUNS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: The caller owns the stable Thread context and service. Scheduling
    // transfers only the kernel continuation after suspend_current_user has
    // withdrawn its CPU-local hardware ownership.
    unsafe { aarch64_run_native_user(active.context.as_ptr()) };
    let publication = local_publication()?;
    if publication.generation.load(Ordering::Acquire) != 0
        || !publication.context.load(Ordering::Relaxed).is_null()
        || !publication.service.load(Ordering::Relaxed).is_null()
        || !publication.syscall_frame.load(Ordering::Relaxed).is_null()
        || context.machine.state != registers::USER_CONTEXT_STATE_STOPPED
    {
        fail_stop();
    }
    stopped_exit(context)
}

fn local_publication() -> Result<&'static RunPublication, Error> {
    ACTIVE_RUNS
        .get(crate::arch::aarch64::current_cpu_index())
        .ok_or(Error::InvalidProcessor)
}

fn publish(active: ActiveRun) -> Result<(), Error> {
    if !crate::arch::aarch64::lower_el::native_world_available() {
        return Err(Error::LowerWorldUnavailable);
    }
    let publication = local_publication()?;
    if publication.generation.load(Ordering::Acquire) != 0
        || !publication.context.load(Ordering::Relaxed).is_null()
        || !publication.service.load(Ordering::Relaxed).is_null()
        || !publication.syscall_frame.load(Ordering::Relaxed).is_null()
    {
        return Err(Error::AlreadyRunning);
    }
    publication
        .context
        .store(active.context.as_ptr(), Ordering::Relaxed);
    publication
        .service
        .store(active.service.as_ptr(), Ordering::Relaxed);
    publication
        .generation
        .store(active.generation, Ordering::Release);
    Ok(())
}

fn withdraw(publication: &RunPublication) {
    publication
        .syscall_frame
        .store(core::ptr::null_mut(), Ordering::Relaxed);
    publication
        .context
        .store(core::ptr::null_mut(), Ordering::Relaxed);
    publication
        .service
        .store(core::ptr::null_mut(), Ordering::Relaxed);
    publication.generation.store(0, Ordering::Release);
}

/// Called only at the masked scheduler boundary before a real context switch.
pub(crate) fn suspend_current_user() {
    let Ok(publication) = local_publication() else {
        fail_stop()
    };
    let generation = publication.generation.load(Ordering::Acquire);
    if generation == 0 {
        return;
    }
    let Ok(mut active) = active_run(generation) else {
        fail_stop()
    };
    let Some(mut frame) = NonNull::new(publication.syscall_frame.load(Ordering::Relaxed)) else {
        // Lower-EL IRQ exits close publication before reaching the scheduler.
        fail_stop();
    };
    // SAFETY: The syscall dispatcher published this private Thread-stack frame
    // before entering interruptible policy and holds no reference during that
    // call. The outgoing Thread is still exclusive and local IRQs are masked.
    let frame = unsafe { frame.as_mut() };
    // SAFETY: The same generation owns this retained context; no reference to
    // it spans policy dispatch or a scheduler transition.
    let context = unsafe { active.context.as_mut() };
    if context.state != registers::USER_CONTEXT_STATE_RUNNING
        || context.run_generation != generation
    {
        fail_stop();
    }
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    SYSCALL_SUSPENSIONS.fetch_add(1, Ordering::Relaxed);
    crate::arch::aarch64::fp::save(&mut context.fp, frame);
    frame.return_cptr &= !registers::CPTR_EL2_FPEN_ENABLED;
    save_thread_registers(context);
    // SAFETY: The original syscall frame lies immediately below its private
    // run anchor. Restore that Thread's host continuation registers before any
    // other Native or guest owner can use this CPU's lower register bank.
    unsafe { aarch64_suspend_native_registers(core::ptr::from_ref(frame)) };
    withdraw(publication);
}

fn prepare_run(context: &mut UserContext, binding: UserRunBinding) -> Result<(), Error> {
    if context.machine.state != registers::USER_CONTEXT_STATE_READY {
        return Err(Error::InvalidMachineState);
    }
    if binding.run_generation() <= context.machine.run_generation {
        return Err(Error::RunGenerationNotIncreasing);
    }
    let current_hcr = read_hcr_el2();
    let entry_hcr = LowerElReturnRegime::Native.transition_hcr(current_hcr)?;
    context.machine.thread = binding.thread();
    context.machine.image_generation = binding.image_generation();
    context.machine.run_generation = binding.run_generation();
    context.machine.exit_kind = registers::USER_CONTEXT_EXIT_NONE;
    context.machine.syndrome = 0;
    context.machine.fault_address = 0;
    context.machine.entry_hcr = entry_hcr;
    context.machine.processor_state &= registers::SPSR_NZCV_MASK;
    context.machine.processor_state |= native_processor_state();
    context.machine.state = registers::USER_CONTEXT_STATE_RUNNING;
    Ok(())
}

const fn native_processor_state() -> u64 {
    // The EL2&0 root contains EL0 and privileged mappings. PAN protects the
    // kernel even on exception paths which retain the process translation.
    registers::SPSR_EL0T
        | registers::SPSR_D
        | registers::SPSR_A
        | registers::SPSR_F
        | registers::SPSR_PAN
}

fn stopped_exit(context: &mut UserContext) -> Result<UserExit<'_>, Error> {
    let binding = current_binding(&context.machine)?;
    let payload = match context.machine.exit_kind {
        registers::USER_CONTEXT_EXIT_FAULT => ExitPayload::Fault(UserFault::new(
            crate::arch::aarch64::user::contract::fault_kind(context.machine.syndrome),
            context.machine.syndrome,
            context.machine.fault_address,
            context.machine.program_counter,
        )),
        registers::USER_CONTEXT_EXIT_INTERRUPTED => ExitPayload::Interrupted,
        _ => return Err(Error::InvalidMachineState),
    };
    let completion = ReturnCapability {
        context: Some(&mut context.machine),
        binding,
    };
    match payload {
        ExitPayload::Fault(fault) => Ok(UserExit::Fault { fault, completion }),
        ExitPayload::Interrupted => Ok(UserExit::Interrupted { completion }),
    }
}

enum ExitPayload {
    Fault(UserFault),
    Interrupted,
}

pub(crate) enum UserExit<'context> {
    Fault {
        fault: UserFault,
        completion: ReturnCapability<'context>,
    },
    Interrupted {
        completion: ReturnCapability<'context>,
    },
}

#[must_use = "native-user return ownership must be resumed or discarded exactly once"]
pub(crate) struct ReturnCapability<'context> {
    context: Option<&'context mut MachineContext>,
    binding: UserRunBinding,
}

impl<'context> ReturnCapability<'context> {
    pub(crate) const fn binding(&self) -> UserRunBinding {
        self.binding
    }

    pub(crate) fn resume_execution(
        mut self,
        expected: UserRunBinding,
    ) -> Result<(), CompletionFailure<'context>> {
        if let Err(error) = self.validate(expected) {
            return Err(CompletionFailure {
                error,
                completion: self,
            });
        }
        let context = self.context_mut();
        context.exit_kind = registers::USER_CONTEXT_EXIT_NONE;
        context.state = registers::USER_CONTEXT_STATE_READY;
        self.context = None;
        Ok(())
    }

    pub(crate) fn discard(
        mut self,
        expected: UserRunBinding,
    ) -> Result<(), CompletionFailure<'context>> {
        if let Err(error) = self.validate(expected) {
            return Err(CompletionFailure {
                error,
                completion: self,
            });
        }
        let context = self.context_mut();
        context.exit_kind = registers::USER_CONTEXT_EXIT_NONE;
        context.state = registers::USER_CONTEXT_STATE_TERMINATED;
        self.context = None;
        Ok(())
    }

    fn validate(&self, expected: UserRunBinding) -> Result<(), Error> {
        let Some(context) = self.context.as_deref() else {
            return Err(Error::InvalidMachineState);
        };
        if self.binding != expected
            || current_binding(context) != Ok(expected)
            || context.state != registers::USER_CONTEXT_STATE_STOPPED
        {
            return Err(Error::CompletionBindingMismatch);
        }
        Ok(())
    }

    fn context_mut(&mut self) -> &mut MachineContext {
        let Some(context) = self.context.as_deref_mut() else {
            fail_stop();
        };
        context
    }
}

impl Drop for ReturnCapability<'_> {
    fn drop(&mut self) {
        if self.context.is_some() {
            fail_stop();
        }
    }
}

#[must_use = "completion failure retains the exactly-once return capability"]
pub(crate) struct CompletionFailure<'context> {
    error: Error,
    completion: ReturnCapability<'context>,
}

impl<'context> CompletionFailure<'context> {
    pub(crate) fn into_parts(self) -> (Error, ReturnCapability<'context>) {
        (self.error, self.completion)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SynchronousAction {
    Resume,
    Unwind,
}

/// Dispatches owned syscall values while retaining the Thread's private frame.
pub(crate) fn handle_synchronous(
    frame: &mut ExceptionFrame,
    generation: u64,
) -> Result<SynchronousAction, Error> {
    let active = active_run(generation)?;
    let class = (frame.esr >> registers::ESR_EC_SHIFT) & registers::ESR_EC_MASK;
    let valid_native_call =
        class == registers::ESR_EC_SVC64 && frame.esr & registers::ESR_ISS_MASK == 0;
    if valid_native_call {
        let invocation = invocation_from_frame(frame)?;
        let frame_pointer = core::ptr::from_mut(frame);
        local_publication()?
            .syscall_frame
            .store(frame_pointer, Ordering::Relaxed);
        // SAFETY: The service and raw frame belong to this stable Thread. No
        // reference into the frame/context or CPU-local slot is used during
        // dispatch. A switch may temporarily access the derived frame pointer
        // under the local IRQ mask before publishing this Thread elsewhere.
        let action = unsafe { active.service.as_ref().handle(invocation) };
        if crate::hal::irq::local_enabled() {
            fail_stop();
        }
        let publication = local_publication()?;
        let detached = publication.generation.load(Ordering::Acquire) == 0;
        if !detached
            && (publication.context.load(Ordering::Relaxed) != active.context.as_ptr()
                || publication.generation.load(Ordering::Relaxed) != generation)
        {
            return Err(Error::InvalidMachineState);
        }
        match action {
            NativeCallAction::Return(result) => {
                if detached {
                    if frame.return_cptr & registers::CPTR_EL2_FPEN_ENABLED != 0 {
                        return Err(Error::InvalidMachineState);
                    }
                    // SAFETY: Suspension saved this exact Thread's TLS and FP
                    // state. The retained run anchor owns its portable host
                    // continuation values; CPU identity lives in TPIDR_EL2.
                    unsafe {
                        aarch64_resume_native_registers(active.context.as_ptr(), frame_pointer)
                    };
                    publish(active)?;
                } else {
                    publication
                        .syscall_frame
                        .store(core::ptr::null_mut(), Ordering::Relaxed);
                }
                #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
                DIRECT_NATIVE_CALLS.fetch_add(1, Ordering::Relaxed);
                frame.general[0] = result.status() as u64;
                frame.general[1] = result.values()[0];
                frame.general[2] = result.values()[1];
                return Ok(SynchronousAction::Resume);
            }
            NativeCallAction::Stop => {
                capture_frame(
                    active.context,
                    generation,
                    frame,
                    registers::USER_CONTEXT_EXIT_INTERRUPTED,
                )?;
                return Ok(SynchronousAction::Unwind);
            }
        }
    }
    capture_frame(
        active.context,
        generation,
        frame,
        registers::USER_CONTEXT_EXIT_FAULT,
    )?;
    Ok(SynchronousAction::Unwind)
}

/// Admits FP only for the exact active Native run, then retries the instruction.
pub(crate) fn restore_fp(frame: &mut ExceptionFrame, generation: u64) -> Result<(), Error> {
    let active = active_run(generation)?;
    // SAFETY: The acquired publication identifies this CPU's current Native
    // owner; IRQ masking excludes any concurrent run-state transition.
    let context = unsafe { active.context.as_ref() };
    if context.run_generation != generation
        || context.state != registers::USER_CONTEXT_STATE_RUNNING
    {
        return Err(Error::InvalidMachineState);
    }
    crate::arch::aarch64::fp::restore(&context.fp, frame).map_err(|()| Error::InvalidMachineState)
}

/// Copies an interrupted native context before a kernel-selected unwind.
pub(crate) fn capture_interrupt(frame: &ExceptionFrame, generation: u64) -> Result<(), Error> {
    let active = active_run(generation)?;
    capture_frame(
        active.context,
        active.generation,
        frame,
        registers::USER_CONTEXT_EXIT_INTERRUPTED,
    )?;
    Ok(())
}

pub(crate) fn active_generation() -> Option<u64> {
    let cpu = crate::arch::aarch64::current_cpu_index();
    ACTIVE_RUNS
        .get(cpu)
        .map(|publication| publication.generation.load(Ordering::Acquire))
        .filter(|generation| *generation != 0)
}

pub(crate) const fn unwind_callback() -> unsafe extern "C" fn() {
    aarch64_unwind_native_user
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn direct_native_call_count_for_test() -> usize {
    DIRECT_NATIVE_CALLS.load(Ordering::Relaxed)
}

#[derive(Clone, Copy)]
struct ActiveRun {
    context: NonNull<MachineContext>,
    generation: u64,
    service: NonNull<NativeCallService<'static>>,
}

fn active_run(generation: u64) -> Result<ActiveRun, Error> {
    if generation == 0 {
        return Err(Error::InvalidMachineState);
    }
    let cpu = crate::arch::aarch64::current_cpu_index();
    let publication = ACTIVE_RUNS.get(cpu).ok_or(Error::InvalidProcessor)?;
    if publication.generation.load(Ordering::Acquire) != generation {
        return Err(Error::InvalidMachineState);
    }
    let pointer = NonNull::new(publication.context.load(Ordering::Relaxed))
        .ok_or(Error::InvalidMachineState)?;
    let service = NonNull::new(publication.service.load(Ordering::Relaxed))
        .ok_or(Error::InvalidMachineState)?;
    Ok(ActiveRun {
        context: pointer,
        generation,
        service,
    })
}

fn invocation_from_frame(frame: &ExceptionFrame) -> Result<NativeInvocation, Error> {
    let call_site = frame
        .elr
        .checked_sub(registers::AARCH64_INSTRUCTION_SIZE)
        .ok_or(Error::InvalidMachineState)?;
    Ok(NativeInvocation::new(
        frame.general[8],
        frame.general[6],
        [
            frame.general[0],
            frame.general[1],
            frame.general[2],
            frame.general[3],
            frame.general[4],
            frame.general[5],
        ],
        call_site,
    ))
}

fn capture_frame(
    mut pointer: NonNull<MachineContext>,
    generation: u64,
    frame: &ExceptionFrame,
    exit_kind: u64,
) -> Result<(), Error> {
    // SAFETY: This pointer was derived from the run's exclusive context borrow
    // and remains retained by its Thread. A detached Stop uses that same
    // identity after suspension; no other Thread may access this context.
    // Capture is bounded by the local IRQ mask and closes publication below.
    let context = unsafe { pointer.as_mut() };
    if context.state != registers::USER_CONTEXT_STATE_RUNNING
        || context.run_generation != generation
    {
        return Err(Error::InvalidMachineState);
    }
    context.general = frame.general;
    context.program_counter = frame.elr;
    context.processor_state = frame.spsr;
    context.stack_pointer = frame.sp_el0;
    let publication = local_publication()?;
    if publication.generation.load(Ordering::Acquire) != 0 {
        if publication.context.load(Ordering::Relaxed) != pointer.as_ptr()
            || publication.generation.load(Ordering::Relaxed) != generation
        {
            return Err(Error::InvalidMachineState);
        }
        crate::arch::aarch64::fp::save(&mut context.fp, frame);
        save_thread_registers(context);
    } else if frame.return_cptr & registers::CPTR_EL2_FPEN_ENABLED != 0 {
        return Err(Error::InvalidMachineState);
    }
    context.exit_kind = exit_kind;
    context.syndrome = frame.esr;
    context.fault_address = frame.far;
    context.state = registers::USER_CONTEXT_STATE_STOPPED;

    withdraw(publication);
    Ok(())
}

fn save_thread_registers(context: &mut MachineContext) {
    // SAFETY: The masked Native owner exclusively retains the live lower-EL
    // thread registers. Its context is accessed only for this bounded capture.
    unsafe {
        asm!(
            "mrs {tpidr_el0}, TPIDR_EL0",
            "mrs {tpidrro_el0}, TPIDRRO_EL0",
            tpidr_el0 = out(reg) context.tpidr_el0,
            tpidrro_el0 = out(reg) context.tpidrro_el0,
            options(nomem, nostack, preserves_flags)
        )
    };
}

fn current_binding(context: &MachineContext) -> Result<UserRunBinding, Error> {
    UserRunBinding::new(
        context.thread,
        context.image_generation,
        context.run_generation,
    )
    .ok_or(Error::InvalidMachineState)
}

fn read_hcr_el2() -> u64 {
    let value: u64;
    // SAFETY: HCR_EL2 is readable at EL2 and has no memory operand.
    unsafe {
        asm!(
            "mrs {value}, HCR_EL2",
            value = out(reg) value,
            options(nomem, nostack, preserves_flags)
        )
    };
    value
}

pub(crate) fn fail_stop() -> ! {
    hyper::debug::invariant_failure("aarch64 Native entry ownership invariant")
}
