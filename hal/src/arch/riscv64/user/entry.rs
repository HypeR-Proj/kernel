// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native U-mode register ownership and contained trap completion.
//! The dedicated vector restores the host vector, zero sscratch, and host
//! gp/tp before Rust. Raw frames never cross the Native policy service.

use crate::arch::riscv64::registers;
use core::arch::asm;
use core::mem::{offset_of, size_of};
use core::ptr::NonNull;
use hyper::abi::native::NativeInvocation;
use hyper::hal::interrupt::{EntryAction, InterruptId, InterruptOrigin};
use hyper::hal::user::{NativeCallAction, NativeCallService, UserFault, UserRunBinding};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
use hyper::sync::atomic::AtomicUsize;
use hyper::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

const READY: u64 = 0;
const RUNNING: u64 = 1;
const STOPPED: u64 = 2;
const TERMINATED: u64 = 3;
const EXIT_NONE: u64 = 0;
const EXIT_FAULT: u64 = 2;
const EXIT_INTERRUPTED: u64 = 3;
const NATIVE_STATE: u64 = (2 << 32) | registers::SSTATUS_SPIE;

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct NativeFrame {
    general: [u64; 32],
    pc: u64,
    status: u64,
    cause: u64,
    fault_address: u64,
}
#[repr(C, align(16))]
struct MachineContext {
    frame: NativeFrame,
    fp: crate::arch::riscv64::fp::State,
    thread: u64,
    image_generation: u64,
    run_generation: u64,
    state: u64,
    exit_kind: u64,
}
pub(crate) struct UserContext {
    machine: MachineContext,
}
impl UserContext {
    pub(crate) fn snapshot_registers(&self, output: &mut hyper::hal::user::UserRegisterSnapshot) {
        output.machine = 243;
        output.general = self.machine.frame.general;
        output.pc = self.machine.frame.pc;
        output.sp = self.machine.frame.general[2];
        output.tls = self.machine.frame.general[4];
        output.processor_state = self.machine.frame.status;
        self.machine.fp.snapshot(output);
    }

    pub(crate) fn set_entry_argument(&mut self, argument: u64) {
        self.machine.frame.general[10] = argument;
    }
    pub(crate) fn try_new(
        entry: u64,
        stack: u64,
        tls: u64,
        address_limit: u64,
    ) -> Result<Self, Error> {
        if entry == 0
            || entry >= address_limit
            || entry & 1 != 0
            || stack == 0
            || stack >= address_limit
            || stack & 15 != 0
            || tls >= address_limit
        {
            return Err(Error::InvalidInitialContext);
        }
        let mut general = [0; 32];
        general[2] = stack;
        general[4] = tls;
        Ok(Self {
            machine: MachineContext {
                frame: NativeFrame {
                    general,
                    pc: entry,
                    status: NATIVE_STATE,
                    cause: 0,
                    fault_address: 0,
                },
                fp: crate::arch::riscv64::fp::State::zeroed(),
                thread: 0,
                image_generation: 0,
                run_generation: 0,
                state: READY,
                exit_kind: EXIT_NONE,
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
    Unsupported(crate::arch::riscv64::user::memory::ContractError),
}
impl From<crate::arch::riscv64::user::memory::ContractError> for Error {
    fn from(value: crate::arch::riscv64::user::memory::ContractError) -> Self {
        Self::Unsupported(value)
    }
}
struct RunPublication {
    generation: AtomicU64,
    context: AtomicPtr<MachineContext>,
    service: AtomicPtr<NativeCallService<'static>>,
    syscall_frame: AtomicPtr<NativeFrame>,
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
static ACTIVE: [RunPublication; hyper::config::MAX_CPUS as usize] =
    [const { RunPublication::new() }; hyper::config::MAX_CPUS as usize];
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
pub(crate) fn direct_native_call_count_for_test() -> usize {
    DIRECT_NATIVE_CALLS.load(Ordering::Relaxed)
}
unsafe extern "C" {
    fn riscv64_run_native_user(context: *mut MachineContext);
    fn riscv64_unwind_native_user();
    fn riscv64_refresh_native_anchor(frame: *const NativeFrame);
    #[link_name = "riscv64_call_native_irq_on_stack"]
    fn call_native_irq_on_stack(
        frame: *mut NativeFrame,
        stack: usize,
        callback: extern "C" fn(&mut NativeFrame) -> u64,
    ) -> u64;
}

/// Runs a stable Thread-owned Native context through sleepable syscall entries.
///
/// # Safety
/// Context and service remain alive at stable addresses until return. Initial
/// entry and final return are IRQ-masked. Scheduler switches withdraw Native
/// machine publication and translation, and syscall return restores translation
/// on the current CPU. No CPU-affine guard may span the call.
pub(crate) unsafe fn run_user<'context>(
    context: &'context mut UserContext,
    binding: UserRunBinding,
    service: &NativeCallService<'_>,
) -> Result<UserExit<'context>, Error> {
    let scratch: u64;
    // SAFETY: A nonzero SSCRATCH names a live lower-world anchor, which Native
    // admission must not replace. The caller keeps local IRQs masked.
    unsafe {
        asm!("csrr {scratch}, sscratch", scratch = out(reg) scratch, options(nomem, nostack))
    };
    if scratch != 0 {
        return Err(Error::LowerWorldUnavailable);
    }
    if context.machine.state != READY {
        return Err(Error::InvalidMachineState);
    }
    if binding.run_generation() <= context.machine.run_generation {
        return Err(Error::RunGenerationNotIncreasing);
    }
    context.machine.thread = binding.thread();
    context.machine.image_generation = binding.image_generation();
    context.machine.run_generation = binding.run_generation();
    context.machine.frame.status = NATIVE_STATE;
    context.machine.exit_kind = EXIT_NONE;
    context.machine.state = RUNNING;
    let active = ActiveRun {
        context: NonNull::from(&mut context.machine),
        service: NonNull::from(service).cast(),
        generation: binding.run_generation(),
    };
    if let Err(error) = publish(active) {
        context.machine.state = READY;
        return Err(error);
    }
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    MACHINE_RUNS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: The caller retains this stable Thread context and service. A
    // switch saves its bank and withdraws CPU publication before transferring
    // the kernel continuation; final exit closes the admitted generation.
    unsafe { riscv64_run_native_user(active.context.as_ptr()) };
    let publication = local_publication()?;
    if publication.generation.load(Ordering::Acquire) != 0
        || !publication.context.load(Ordering::Relaxed).is_null()
        || !publication.service.load(Ordering::Relaxed).is_null()
        || !publication.syscall_frame.load(Ordering::Relaxed).is_null()
        || context.machine.state != STOPPED
    {
        fail_stop();
    }
    stopped_exit(context)
}

fn local_publication() -> Result<&'static RunPublication, Error> {
    ACTIVE
        .get(crate::arch::riscv64::current_cpu_index())
        .ok_or(Error::InvalidProcessor)
}

fn publish(active: ActiveRun) -> Result<(), Error> {
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

/// Called only at a masked scheduler boundary before switching Threads.
pub(crate) fn suspend_current_user() {
    let Ok(publication) = local_publication() else {
        fail_stop()
    };
    if publication.generation.load(Ordering::Acquire) == 0 {
        return;
    }
    let Ok(mut active) = active_run() else {
        fail_stop()
    };
    let Some(mut frame) = NonNull::new(publication.syscall_frame.load(Ordering::Relaxed)) else {
        fail_stop()
    };
    // SAFETY: Policy holds no frame reference. This derived pointer belongs to
    // the outgoing Thread's retained syscall stack; IRQ masking and scheduler
    // ownership exclude any concurrent access or resumed execution.
    let frame = unsafe { frame.as_mut() };
    // SAFETY: The generation identifies the same exclusively retained context;
    // no context reference survives interruptible policy dispatch.
    let context = unsafe { active.context.as_mut() };
    if context.state != RUNNING || context.run_generation != active.generation {
        fail_stop();
    }
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    SYSCALL_SUSPENSIONS.fetch_add(1, Ordering::Relaxed);
    crate::arch::riscv64::fp::save(&mut context.fp, frame.status);
    frame.status &= !registers::SSTATUS_FS_MASK;
    withdraw(publication);
}

fn stopped_exit(context: &mut UserContext) -> Result<UserExit<'_>, Error> {
    let binding = current_binding(&context.machine)?;
    let payload = match context.machine.exit_kind {
        EXIT_FAULT => ExitPayload::Fault(UserFault::new(
            crate::arch::riscv64::user::contract::fault_kind(context.machine.frame.cause),
            context.machine.frame.cause,
            context.machine.frame.fault_address,
            context.machine.frame.pc,
        )),
        EXIT_INTERRUPTED => ExitPayload::Interrupted,
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
        context.exit_kind = EXIT_NONE;
        context.state = READY;
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
        context.exit_kind = EXIT_NONE;
        context.state = TERMINATED;
        self.context = None;
        Ok(())
    }

    fn validate(&self, expected: UserRunBinding) -> Result<(), Error> {
        let Some(context) = self.context.as_deref() else {
            return Err(Error::InvalidMachineState);
        };
        if self.binding != expected
            || current_binding(context) != Ok(expected)
            || context.state != STOPPED
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

#[derive(Clone, Copy)]
struct ActiveRun {
    context: NonNull<MachineContext>,
    service: NonNull<NativeCallService<'static>>,
    generation: u64,
}
fn active_run() -> Result<ActiveRun, Error> {
    let publication = ACTIVE
        .get(crate::arch::riscv64::current_cpu_index())
        .ok_or(Error::InvalidProcessor)?;
    let generation = publication.generation.load(Ordering::Acquire);
    if generation == 0 {
        return Err(Error::InvalidMachineState);
    }
    Ok(ActiveRun {
        context: NonNull::new(publication.context.load(Ordering::Relaxed))
            .ok_or(Error::InvalidMachineState)?,
        service: NonNull::new(publication.service.load(Ordering::Relaxed))
            .ok_or(Error::InvalidMachineState)?,
        generation,
    })
}
fn invocation(frame: &NativeFrame) -> Result<NativeInvocation, Error> {
    Ok(NativeInvocation::new(
        frame.general[17],
        frame.general[16],
        [
            frame.general[10],
            frame.general[11],
            frame.general[12],
            frame.general[13],
            frame.general[14],
            frame.general[15],
        ],
        frame.pc.checked_sub(4).ok_or(Error::InvalidMachineState)?,
    ))
}
fn capture(mut active: ActiveRun, frame: &NativeFrame, exit_kind: u64) -> Result<(), Error> {
    // SAFETY: This identity was acquired while the run was published and stays
    // owned by its Thread across suspension. No reference into the context
    // is used during policy dispatch; this bounded capture is IRQ-masked.
    let context = unsafe { active.context.as_mut() };
    if context.state != RUNNING || context.run_generation != active.generation {
        return Err(Error::InvalidMachineState);
    }
    let publication = local_publication()?;
    if publication.generation.load(Ordering::Acquire) != 0 {
        if publication.context.load(Ordering::Relaxed) != active.context.as_ptr()
            || publication.generation.load(Ordering::Relaxed) != active.generation
        {
            return Err(Error::InvalidMachineState);
        }
        crate::arch::riscv64::fp::save(&mut context.fp, frame.status);
    } else if frame.status & registers::SSTATUS_FS_MASK != 0 {
        return Err(Error::InvalidMachineState);
    }
    context.frame = *frame;
    context.exit_kind = exit_kind;
    context.state = STOPPED;
    withdraw(publication);
    Ok(())
}

#[unsafe(no_mangle)]
extern "C" fn riscv64_native_trap_dispatch(frame: &mut NativeFrame) -> u64 {
    // This vector is installed only during an admitted U run. A supervisor
    // exception during assembly transition is corruption, never a user fault.
    if frame.status & registers::SSTATUS_SPP != 0 {
        fail_stop();
    }
    if frame.cause >> 63 != 0 {
        let top = crate::arch::riscv64::exception::native_irq_stack_top();
        // SAFETY: Entry restored host gp/tp, stvec and zero sscratch. The
        // dedicated IRQ stack is live and unused on this interrupt-masked CPU.
        return unsafe {
            call_native_irq_on_stack(core::ptr::from_mut(frame), top, dispatch_native_irq)
        };
    }
    match dispatch_synchronous(frame) {
        Ok(action) => action,
        Err(_) => fail_stop(),
    }
}
fn dispatch_synchronous(frame: &mut NativeFrame) -> Result<u64, Error> {
    let mut active = active_run()?;
    if frame.cause == 2 && frame.status & registers::SSTATUS_FS_MASK == 0 {
        // SAFETY: The generation-qualified publication owns this masked run.
        let context = unsafe { active.context.as_mut() };
        if context.state != RUNNING || context.run_generation != active.generation {
            return Err(Error::InvalidMachineState);
        }
        crate::arch::riscv64::fp::restore(&context.fp, &mut frame.status)
            .map_err(|_| Error::InvalidMachineState)?;
        return Ok(0);
    }
    if frame.cause == 8 {
        // ECALL is always four bytes, even with compressed instructions enabled.
        frame.pc = frame.pc.checked_add(4).ok_or(Error::InvalidMachineState)?;
        let call = invocation(frame)?;
        let frame_pointer = core::ptr::from_mut(frame);
        local_publication()?
            .syscall_frame
            .store(frame_pointer, Ordering::Relaxed);
        // SAFETY: The service and private frame follow this stable Thread.
        // No frame/context reference or CPU-local slot borrow is used while
        // policy runs interruptibly; suspension accesses only the derived raw
        // pointer under the masked outgoing scheduler boundary.
        let action = unsafe { active.service.as_ref().handle(call) };
        if crate::hal::irq::local_enabled() {
            fail_stop();
        }
        let publication = local_publication()?;
        let detached = publication.generation.load(Ordering::Acquire) == 0;
        if !detached
            && (publication.context.load(Ordering::Relaxed) != active.context.as_ptr()
                || publication.generation.load(Ordering::Relaxed) != active.generation)
        {
            return Err(Error::InvalidMachineState);
        }
        if detached {
            // SAFETY: TP is this hart's kernel CPU identity. Refresh the private
            // anchor before either a later U trap or final unwind can restore
            // it. User TP remains exclusively in the original frame's x4.
            unsafe { riscv64_refresh_native_anchor(frame_pointer) };
        }
        match action {
            NativeCallAction::Return(result) => {
                if detached {
                    if frame.status & registers::SSTATUS_FS_MASK != 0 {
                        return Err(Error::InvalidMachineState);
                    }
                    publish(active)?;
                } else {
                    publication
                        .syscall_frame
                        .store(core::ptr::null_mut(), Ordering::Relaxed);
                }
                frame.general[10] = result.status() as u64;
                frame.general[11] = result.values()[0];
                frame.general[12] = result.values()[1];
                #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
                DIRECT_NATIVE_CALLS.fetch_add(1, Ordering::Relaxed);
                return Ok(0);
            }
            NativeCallAction::Stop => capture(active, frame, EXIT_INTERRUPTED)?,
        }
    } else {
        capture(active, frame, EXIT_FAULT)?;
    }
    Ok(1)
}
extern "C" fn dispatch_native_irq(frame: &mut NativeFrame) -> u64 {
    let origin = InterruptOrigin::Native {
        unwind: riscv64_unwind_native_user,
    };
    let action = match frame.cause & !(1 << 63) {
        1 => {
            crate::arch::riscv64::irq::clear_software_interrupt();
            crate::arch::irq::service_kernel_rpc_interrupt(origin)
        }
        5 => crate::arch::irq::dispatch_entry(
            InterruptId::new(0),
            origin,
            &crate::hal::irq::InterruptFrame::unavailable(),
        ),
        9 => match crate::arch::irq::claim_and_dispatch_external_entry(
            origin,
            &crate::hal::irq::InterruptFrame::unavailable(),
        ) {
            Some(action) => action,
            None => return 0,
        },
        _ => fail_stop(),
    };
    match action {
        EntryAction::Resume { postlude: None } => 0,
        EntryAction::Resume {
            postlude: Some(target),
        } => {
            if target as usize != riscv64_unwind_native_user as *const () as usize {
                fail_stop();
            }
            let active = match active_run() {
                Ok(active) => active,
                Err(_) => fail_stop(),
            };
            if capture(active, frame, EXIT_INTERRUPTED).is_err() {
                fail_stop();
            }
            // Assembly invokes the qualified postlude after the IRQ stack and
            // all frame borrows are gone. It restores the original host anchor.
            1
        }
        EntryAction::StopGuest { .. } | EntryAction::Stop => fail_stop(),
    }
}
fn current_binding(context: &MachineContext) -> Result<UserRunBinding, Error> {
    UserRunBinding::new(
        context.thread,
        context.image_generation,
        context.run_generation,
    )
    .ok_or(Error::InvalidMachineState)
}

fn fail_stop() -> ! {
    hyper::debug::invariant_failure("riscv64 Native entry ownership invariant")
}
const _: () = {
    assert!(offset_of!(NativeFrame, general) == 0);
    assert!(offset_of!(NativeFrame, pc) == registers::USER_FRAME_PC_OFFSET as usize);
    assert!(offset_of!(NativeFrame, status) == registers::USER_FRAME_STATUS_OFFSET as usize);
    assert!(offset_of!(NativeFrame, cause) == registers::USER_FRAME_CAUSE_OFFSET as usize);
    assert!(offset_of!(NativeFrame, fault_address) == registers::USER_FRAME_FAULT_OFFSET as usize);
    assert!(size_of::<NativeFrame>() == registers::USER_FRAME_SIZE as usize);
    assert!(offset_of!(MachineContext, frame) == 0);
};
