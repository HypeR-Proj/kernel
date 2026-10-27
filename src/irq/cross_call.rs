// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Typed kernel RPC transport over one architecture-reserved doorbell.
//!
//! The mailbox is allocation-free and generation tagged. A route rejection or
//! missing acknowledgement is ambiguous, so the transport poisons itself and
//! enters fail-stop instead of reusing payload storage under a late handler.

use core::cell::UnsafeCell;

use hyper::cpu::{CpuIndex, PerCpu};
use hyper::hal::interrupt::{InterruptId, InterruptPriority, InterruptTrigger, KernelRpcReasons};
use hyper::sync::GenerationTaggedState;
use hyper::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

const TIMEOUT_NS: u64 = 1_000_000_000;
const IDLE: u8 = 0;
const PENDING: u8 = 1;
const WORKING: u8 = 2;
const APPLIED: u8 = 3;
const REJECTED: u8 = 4;
const APPLIED_OR_UNKNOWN: u8 = 5;

static OWNER: AtomicBool = AtomicBool::new(false);
static PUBLISHED_GENERATION: AtomicU32 = AtomicU32::new(0);
static POISONED: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU32 = AtomicU32::new(0);
static OPCODE: AtomicU8 = AtomicU8::new(0);
static ARGUMENT0: AtomicU32 = AtomicU32::new(0);
static ARGUMENT1: AtomicU8 = AtomicU8::new(0);
static ARGUMENT2: AtomicU8 = AtomicU8::new(0);
static STATUS: PerCpu<AtomicU64> = PerCpu::new([const { AtomicU64::new(0) }; hyper::cpu::MAX_CPUS]);
static ACK: PerCpu<AtomicU32> = PerCpu::new([const { AtomicU32::new(0) }; hyper::cpu::MAX_CPUS]);

struct UserAddressSpacePayload(UnsafeCell<Option<UserAddressSpaceCall>>);

// SAFETY: OWNER serializes publishers. PUBLISHED_GENERATION Release/Acquire
// publishes the payload to handlers, while generation-tagged STATUS prevents
// a delayed handler from claiming a later request. The publisher waits for
// every target acknowledgement before clearing or replacing the payload.
unsafe impl Sync for UserAddressSpacePayload {}

static USER_ADDRESS_SPACE_PAYLOAD: UserAddressSpacePayload =
    UserAddressSpacePayload(UnsafeCell::new(None));

struct GuestStage2Payload(UnsafeCell<Option<GuestStage2Call>>);

// SAFETY: The same OWNER and exact-generation protocol described for the user
// payload protects this separate, immutable guest-retirement payload.
unsafe impl Sync for GuestStage2Payload {}

static GUEST_STAGE2_PAYLOAD: GuestStage2Payload = GuestStage2Payload(UnsafeCell::new(None));

#[derive(Clone, Copy)]
pub(crate) enum LocalIrqOperation {
    Configure,
    Enable,
    Disable,
}

#[derive(Clone, Copy)]
pub(crate) enum KernelRpc {
    LocalIrqLifecycle {
        hardware: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
        operation: LocalIrqOperation,
    },
    UserAddressSpace(UserAddressSpaceCall),
    GuestStage2(GuestStage2Call),
}

#[derive(Clone, Copy)]
pub(crate) struct UserAddressSpaceCall {
    owner: u64,
    epoch: u64,
    new_epoch: u64,
    active_target: bool,
    expected_active: crate::hal::user::LocalIdentity,
    request: crate::hal::user::LocalRequest,
}

#[derive(Clone, Copy)]
pub(crate) struct GuestStage2Call {
    request: crate::vm::memory::GuestStage2LocalRequest,
}

pub(crate) enum UserAddressSpaceOperation<'root> {
    Replace(&'root crate::hal::user::PreparedAddressSpace),
    Invalidate(&'root crate::hal::user::PreparedAddressSpace),
}

pub(crate) struct UserAddressSpaceExecution<'root> {
    pub(crate) owner: u64,
    pub(crate) epoch: u64,
    pub(crate) new_epoch: u64,
    pub(crate) active_target: bool,
    pub(crate) expected: &'root crate::hal::user::PreparedAddressSpace,
    pub(crate) operation: UserAddressSpaceOperation<'root>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LocalApply {
    Applied,
    Rejected,
    AppliedOrUnknown,
}

pub(crate) struct Outcome {
    pub(crate) rejected_cpu: Option<usize>,
    pub(crate) ambiguous_cpu: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AcquireError {
    Busy,
    Unavailable,
}

/// Exclusive mailbox owner pinned to the CPU which acquired it.
///
/// A user-address-space transaction can span more than one RPC publication.
/// Keeping the pin in this outer owner closes the otherwise-valid migration
/// window between those publications and guarantees that a competing thread
/// can make progress on another CPU while this transaction remains active.
struct Owner {
    pin: Option<crate::task::scheduler::PreemptionGuard>,
}

impl Owner {
    fn acquire() -> Result<Self, AcquireError> {
        let pin =
            crate::task::scheduler::preempt_disable().map_err(|_| AcquireError::Unavailable)?;
        if POISONED.load(Ordering::Acquire) {
            release_owner_pin(pin);
            return Err(AcquireError::Unavailable);
        }
        match OWNER.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => Ok(Self { pin: Some(pin) }),
            Err(_) => {
                release_owner_pin(pin);
                Err(AcquireError::Busy)
            }
        }
    }
}

/// Linear reservation for a multi-phase user-translation transaction.
///
/// Acquiring this before logical publication makes ordinary transport
/// contention recoverable. Once publication occurs, every execution either
/// acknowledges or enters fail-stop without releasing retained roots.
pub(crate) struct UserAddressSpaceTransaction {
    _owner: Owner,
}

/// Linear reservation for one final guest stage-2 invalidation transaction.
///
/// The caller acquires this before cutting address-space residency. Once the
/// cut is published, rejection, ambiguity, or timeout fail-stop inside the
/// transport while the VM owner and retiring VMID remain retained.
pub(crate) struct GuestStage2Transaction {
    _owner: Owner,
}

impl GuestStage2Transaction {
    pub(crate) fn try_acquire() -> Result<Self, ()> {
        Owner::acquire()
            .map(|owner| Self { _owner: owner })
            .map_err(|_| ())
    }

    pub(crate) fn execute(
        &mut self,
        request: crate::vm::memory::GuestStage2LocalRequest,
        count: usize,
        targets: &[bool; hyper::cpu::MAX_CPUS],
    ) -> Outcome {
        execute_owned(
            KernelRpc::GuestStage2(GuestStage2Call { request }),
            count,
            targets,
        )
    }
}

impl UserAddressSpaceTransaction {
    pub(crate) fn try_acquire() -> Result<Self, ()> {
        Owner::acquire()
            .map(|owner| Self { _owner: owner })
            .map_err(|_| ())
    }

    /// Waits for ordinary mailbox contention from a sleepable kernel context.
    ///
    /// Contention is not a machine failure: concurrent process loaders can
    /// legitimately update independent address spaces. Poisoned transport or
    /// a context which cannot yield remains an immediate error.
    pub(crate) fn acquire_waiting() -> Result<Self, ()> {
        loop {
            match Owner::acquire() {
                Ok(owner) => return Ok(Self { _owner: owner }),
                Err(AcquireError::Busy) => {
                    crate::task::scheduler::yield_now().map_err(|_| ())?;
                }
                Err(AcquireError::Unavailable) => return Err(()),
            }
        }
    }

    pub(crate) fn execute(
        &mut self,
        execution: UserAddressSpaceExecution<'_>,
        count: usize,
        targets: &[bool; hyper::cpu::MAX_CPUS],
    ) -> Outcome {
        // SAFETY: Both roots are borrowed through this synchronous execution,
        // and the transaction cannot release the mailbox until every target
        // acknowledged or fail-stop prevented normal return.
        let request = unsafe {
            match execution.operation {
                UserAddressSpaceOperation::Replace(root) => root.replace_request(),
                UserAddressSpaceOperation::Invalidate(root) => root.invalidate_request(),
            }
        };
        let call = UserAddressSpaceCall {
            owner: execution.owner,
            epoch: execution.epoch,
            new_epoch: execution.new_epoch,
            active_target: execution.active_target,
            expected_active: execution.expected.local_identity(),
            request,
        };
        execute_owned(KernelRpc::UserAddressSpace(call), count, targets)
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        PUBLISHED_GENERATION.store(0, Ordering::Release);
        // SAFETY: This owner observed every target acknowledgement before a
        // normal return. A zero published generation excludes new claims, and
        // generation-tagged status rejects every delayed old claim.
        unsafe { *USER_ADDRESS_SPACE_PAYLOAD.0.get() = None };
        // SAFETY: The same exact-generation exclusion applies to this payload.
        unsafe {
            *GUEST_STAGE2_PAYLOAD.0.get() = None;
        }
        OWNER.store(false, Ordering::Release);
        let Some(pin) = self.pin.take() else {
            poison("kernel RPC owner pin is missing");
        };
        release_owner_pin(pin);
    }
}

fn release_owner_pin(pin: crate::task::scheduler::PreemptionGuard) {
    // Mailbox ownership is an inner synchronization protocol, not a scheduling
    // point. The enclosing kernel path observes any deferred request after its
    // own linear state has been committed or rolled back.
    if crate::task::scheduler::preempt_enable_without_reschedule(pin).is_err() {
        poison("kernel RPC owner pin release failed");
    }
}

pub(crate) fn execute(
    rpc: KernelRpc,
    count: usize,
    targets: &[bool; hyper::cpu::MAX_CPUS],
) -> Result<Outcome, ()> {
    let _owner = Owner::acquire().map_err(|_| ())?;
    Ok(execute_owned(rpc, count, targets))
}

fn execute_owned(rpc: KernelRpc, count: usize, targets: &[bool; hyper::cpu::MAX_CPUS]) -> Outcome {
    let generation = next_generation();
    publish(rpc);
    GENERATION.store(generation, Ordering::Relaxed);
    initialize_target_status(generation, count, targets);
    PUBLISHED_GENERATION.store(generation, Ordering::Release);
    service_local_irq_mailbox();
    notify_remote_targets(rpc, count, targets);
    await_acknowledgements(generation, count, targets);
    PUBLISHED_GENERATION.store(0, Ordering::Release);
    collect_outcome(rpc, generation, count, targets)
}

fn next_generation() -> u32 {
    match GENERATION.load(Ordering::Relaxed).checked_add(1) {
        Some(generation) if generation != 0 => generation,
        _ => poison("kernel RPC generation exhausted"),
    }
}

fn initialize_target_status(generation: u32, count: usize, targets: &[bool; hyper::cpu::MAX_CPUS]) {
    for (cpu, targeted) in targets.iter().copied().enumerate() {
        let Some(cpu) = CpuIndex::new(cpu) else {
            continue;
        };
        let state = if cpu.get() < count && targeted {
            PENDING
        } else {
            IDLE
        };
        STATUS[cpu].store(
            GenerationTaggedState::new(generation, state).bits(),
            Ordering::Relaxed,
        );
        ACK[cpu].store(0, Ordering::Relaxed);
    }
}

fn notify_remote_targets(rpc: KernelRpc, count: usize, targets: &[bool; hyper::cpu::MAX_CPUS]) {
    let current = crate::cpu::current_index();
    for (cpu, targeted) in targets.iter().copied().enumerate().take(count) {
        let Some(cpu) = CpuIndex::new(cpu) else {
            continue;
        };
        if !targeted || Some(cpu) == current {
            continue;
        }
        if !crate::hal::irq::notify_kernel_rpc(cpu, rpc.reason()) {
            poison("kernel RPC route rejected");
        }
    }
}

fn await_acknowledgements(generation: u32, count: usize, targets: &[bool; hyper::cpu::MAX_CPUS]) {
    match crate::time::spin_wait_until(TIMEOUT_NS, || {
        service();
        targets
            .iter()
            .copied()
            .enumerate()
            .take(count)
            .all(|(index, targeted)| {
                !targeted
                    || CpuIndex::new(index)
                        .is_some_and(|cpu| ACK[cpu].load(Ordering::Acquire) == generation)
            })
    }) {
        Ok(true) => {}
        Ok(false) | Err(_) => poison("kernel RPC acknowledgement timed out"),
    }
}

fn collect_outcome(
    rpc: KernelRpc,
    generation: u32,
    count: usize,
    targets: &[bool; hyper::cpu::MAX_CPUS],
) -> Outcome {
    let mut rejected_cpu = None;
    let mut ambiguous_cpu = match rpc {
        KernelRpc::UserAddressSpace(_) => first_target_outside_count(count, targets),
        KernelRpc::GuestStage2(_) => first_target_outside_count(count, targets),
        KernelRpc::LocalIrqLifecycle { .. } => None,
    };
    for (index, targeted) in targets.iter().copied().enumerate().take(count) {
        if !targeted {
            continue;
        }
        let Some(cpu) = CpuIndex::new(index) else {
            continue;
        };
        let status = GenerationTaggedState::from_bits(STATUS[cpu].load(Ordering::Acquire));
        let status = match status.state_for(generation) {
            Some(status) => status,
            None => poison("kernel RPC completion generation is inconsistent"),
        };
        match status {
            APPLIED => {}
            REJECTED => {
                if rejected_cpu.is_none() {
                    rejected_cpu = Some(index);
                }
            }
            APPLIED_OR_UNKNOWN => {
                if ambiguous_cpu.is_none() {
                    ambiguous_cpu = Some(index);
                }
            }
            _ => poison("kernel RPC completion state is inconsistent"),
        }
    }
    Outcome {
        rejected_cpu,
        ambiguous_cpu,
    }
}

fn first_target_outside_count(
    count: usize,
    targets: &[bool; hyper::cpu::MAX_CPUS],
) -> Option<usize> {
    targets
        .iter()
        .copied()
        .enumerate()
        .skip(count)
        .find_map(|(index, targeted)| targeted.then_some(index))
}

impl KernelRpc {
    const fn reason(self) -> KernelRpcReasons {
        match self {
            Self::LocalIrqLifecycle { .. } => KernelRpcReasons::LOCAL_IRQ_LIFECYCLE,
            Self::UserAddressSpace(_) => KernelRpcReasons::USER_ADDRESS_SPACE,
            Self::GuestStage2(_) => KernelRpcReasons::GUEST_STAGE2,
        }
    }
}

pub(crate) fn service() {
    loop {
        let reasons = crate::hal::irq::take_kernel_rpc_reasons();
        if reasons.has_unknown() {
            poison("kernel RPC reason mailbox contains unknown bits");
        }
        if reasons.is_empty() {
            return;
        }
        if reasons.contains(KernelRpcReasons::STAGE1_TLB_SHOOTDOWN) {
            let _ = crate::hal::memory::service_stage1_tlb_shootdown();
        }
        if reasons.contains(KernelRpcReasons::LOCAL_IRQ_LIFECYCLE) {
            service_local_irq_mailbox();
        }
        if reasons.contains(KernelRpcReasons::USER_ADDRESS_SPACE) {
            service_local_irq_mailbox();
        }
        if reasons.contains(KernelRpcReasons::GUEST_STAGE2) {
            service_local_irq_mailbox();
        }
        #[cfg(all(CONFIG_ARCH_AARCH64, feature = "pmu-profiling"))]
        if reasons.contains(KernelRpcReasons::PMU_CONTROL) {
            crate::diagnostics::profile::service_control();
        }
    }
}

fn service_local_irq_mailbox() {
    let generation = PUBLISHED_GENERATION.load(Ordering::Acquire);
    if generation == 0 {
        return;
    }
    let Some(cpu) = crate::cpu::current_index() else {
        return;
    };
    if STATUS[cpu]
        .compare_exchange(
            GenerationTaggedState::new(generation, PENDING).bits(),
            GenerationTaggedState::new(generation, WORKING).bits(),
            Ordering::Acquire,
            Ordering::Relaxed,
        )
        .is_err()
    {
        return;
    }
    let outcome = match decode() {
        KernelRpc::LocalIrqLifecycle {
            hardware,
            priority,
            trigger,
            operation,
        } => match super::interrupt::apply_local_irq_lifecycle(
            hardware, priority, trigger, operation,
        ) {
            super::interrupt::LocalLifecycleApply::Applied => LocalApply::Applied,
            super::interrupt::LocalLifecycleApply::Rejected => LocalApply::Rejected,
            super::interrupt::LocalLifecycleApply::AppliedOrUnknown => LocalApply::AppliedOrUnknown,
        },
        KernelRpc::UserAddressSpace(request) => crate::mm::user_space::service_local_rpc(
            request.owner,
            request.epoch,
            request.new_epoch,
            request.active_target,
            request.expected_active,
            request.request,
        ),
        KernelRpc::GuestStage2(request) => {
            crate::vm::memory::service_local_retirement(request.request);
            LocalApply::Applied
        }
    };
    let status = match outcome {
        LocalApply::Applied => APPLIED,
        LocalApply::Rejected => REJECTED,
        LocalApply::AppliedOrUnknown => APPLIED_OR_UNKNOWN,
    };
    STATUS[cpu].store(
        GenerationTaggedState::new(generation, status).bits(),
        Ordering::Relaxed,
    );
    ACK[cpu].store(generation, Ordering::Release);
}

fn publish(rpc: KernelRpc) {
    match rpc {
        KernelRpc::LocalIrqLifecycle {
            hardware,
            priority,
            trigger,
            operation,
        } => {
            ARGUMENT0.store(hardware.get(), Ordering::Relaxed);
            ARGUMENT1.store(encode_priority(priority), Ordering::Relaxed);
            ARGUMENT2.store(encode_trigger(trigger), Ordering::Relaxed);
            OPCODE.store(encode_operation(operation), Ordering::Relaxed);
        }
        KernelRpc::UserAddressSpace(request) => {
            // SAFETY: The unique OWNER writes while no generation is
            // published. The payload is Copy and remains immutable until all
            // targets acknowledge the following generation publication.
            unsafe { *USER_ADDRESS_SPACE_PAYLOAD.0.get() = Some(request) };
            OPCODE.store(3, Ordering::Relaxed);
        }
        KernelRpc::GuestStage2(request) => {
            // SAFETY: OWNER excludes another publisher and the payload remains
            // immutable through every exact-generation acknowledgement.
            unsafe { *GUEST_STAGE2_PAYLOAD.0.get() = Some(request) };
            OPCODE.store(4, Ordering::Relaxed);
        }
    }
}

fn decode() -> KernelRpc {
    let opcode = OPCODE.load(Ordering::Relaxed);
    if opcode == 4 {
        // SAFETY: Exact-generation Acquire publication and OWNER retention
        // provide the same immutable payload proof as user-address-space RPC.
        let payload = unsafe { *GUEST_STAGE2_PAYLOAD.0.get() };
        return match payload {
            Some(request) => KernelRpc::GuestStage2(request),
            None => poison("kernel RPC guest stage-2 payload is missing"),
        };
    }
    if opcode == 3 {
        // SAFETY: PUBLISHED_GENERATION Acquire precedes the exact-generation
        // status claim, and the publishing OWNER keeps this Copy payload
        // immutable until this CPU acknowledges completion.
        let payload = unsafe { *USER_ADDRESS_SPACE_PAYLOAD.0.get() };
        return match payload {
            Some(request) => KernelRpc::UserAddressSpace(request),
            None => poison("kernel RPC user-address-space payload is missing"),
        };
    }
    KernelRpc::LocalIrqLifecycle {
        hardware: InterruptId::new(ARGUMENT0.load(Ordering::Relaxed)),
        priority: decode_priority(ARGUMENT1.load(Ordering::Relaxed)),
        trigger: if ARGUMENT2.load(Ordering::Relaxed) == 0 {
            InterruptTrigger::Level
        } else {
            InterruptTrigger::Edge
        },
        operation: match opcode {
            0 => LocalIrqOperation::Configure,
            1 => LocalIrqOperation::Enable,
            _ => LocalIrqOperation::Disable,
        },
    }
}

fn poison(reason: &'static str) -> ! {
    POISONED.store(true, Ordering::Release);
    crate::diagnostics::crash::fatal(format_args!(
        "HypeR: poisoned kernel RPC transport: {reason}"
    ))
}

const fn encode_priority(priority: InterruptPriority) -> u8 {
    match priority {
        InterruptPriority::Critical => 0,
        InterruptPriority::High => 1,
        InterruptPriority::Normal => 2,
        InterruptPriority::Low => 3,
    }
}

const fn decode_priority(priority: u8) -> InterruptPriority {
    match priority {
        0 => InterruptPriority::Critical,
        1 => InterruptPriority::High,
        2 => InterruptPriority::Normal,
        _ => InterruptPriority::Low,
    }
}

const fn encode_trigger(trigger: InterruptTrigger) -> u8 {
    match trigger {
        InterruptTrigger::Level => 0,
        InterruptTrigger::Edge => 1,
    }
}

const fn encode_operation(operation: LocalIrqOperation) -> u8 {
    match operation {
        LocalIrqOperation::Configure => 0,
        LocalIrqOperation::Enable => 1,
        LocalIrqOperation::Disable => 2,
    }
}
