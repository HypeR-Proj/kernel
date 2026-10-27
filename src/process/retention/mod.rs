// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Supervisor-owned, bounded postmortem evidence. No retained owner is active
//! process authority; releasing the controller revokes every derived view.

mod policy;

use super::{Process, ProcessError, TerminalReason};
use crate::authority::Rights;
use crate::capability::HandleSnapshot;
use crate::hal::user::UserRegisterSnapshot;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
};
use {
    crate::mm::user_space::CrashMapping, crate::mm::user_space::CrashReadError,
    crate::mm::user_space::NativeAddressSpace, crate::mm::user_space::NativeCrashMemory,
};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::SignalMask,
    crate::object::SignalSource, crate::object::SignalState, crate::object::TransferClass,
    crate::object::object_allocation_size, crate::object::private,
};

type Lock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;
const MAX_THREADS: usize = 32;
const MAX_HANDLES: usize = 256;
pub(crate) const RELEASE_EXPLICIT: u32 = 1;
const RELEASE_OWNER_LOST: u32 = 2;
const RELEASE_DEADLINE: u32 = 3;
pub(super) const RELEASE_ABORTED: u32 = 4;
const READY: SignalMask = SignalMask::from_trusted_bits(1);
const RELEASED: SignalMask = SignalMask::from_trusted_bits(2);

#[derive(Debug)]
pub(crate) enum Error {
    Process(ProcessError),
    Invalid,
    Busy,
    Revoked,
    NotFound,
    Limit,
}
impl From<ProcessError> for Error {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

pub(super) struct SupervisorBudget {
    state: Lock<policy::Budget>,
    _charge: CommittedCharge,
}
impl SupervisorBudget {
    pub(super) fn new(sponsor: &ResourceDomain) -> Result<FallibleArc<Self>, Error> {
        let charge = charge(sponsor, FallibleArc::<Self>::allocation_size())?;
        FallibleArc::try_new(Self {
            state: Lock::new(policy::Budget::default()),
            _charge: charge,
        })
        .map_err(|_| Error::Process(ProcessError::Allocation))
    }
}

struct BudgetOwner {
    budget: FallibleArc<SupervisorBudget>,
    armed: AtomicBool,
}
impl BudgetOwner {
    fn disarm(&self) {
        if self.armed.swap(false, Ordering::AcqRel) {
            self.budget.state.with(policy::Budget::disarm);
        }
    }
}
impl Drop for BudgetOwner {
    fn drop(&mut self) {
        self.disarm();
    }
}
struct RetainedBudget {
    budget: FallibleArc<SupervisorBudget>,
    bytes: u64,
}
struct RetainedMemory {
    memory: Option<NativeCrashMemory>,
    // The budget follows admitted readers until their bounded copy finishes.
    // Revoking the view cannot make those still-pinned pages unaccounted.
    _budget: RetainedBudget,
    _charge: CommittedCharge,
}
impl Drop for RetainedBudget {
    fn drop(&mut self) {
        self.budget.state.with(|state| state.release(self.bytes));
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ThreadEvidence {
    pub(crate) koid: u64,
    pub(crate) registers: UserRegisterSnapshot,
}
#[derive(Clone, Copy)]
pub(crate) struct Info {
    pub(crate) phase: u32,
    pub(crate) flags: u32,
    pub(crate) release_reason: u32,
    pub(crate) process_koid: u64,
    pub(crate) deadline_ns: u64,
    pub(crate) retained_bytes: u64,
    pub(crate) memory_limit: u64,
    pub(crate) terminal: Option<TerminalReason>,
    pub(crate) fault_pc: u64,
    pub(crate) fault_address: u64,
    pub(crate) fault_syndrome: u64,
    pub(crate) thread_count: usize,
    pub(crate) mapping_count: usize,
    pub(crate) handle_count: usize,
}
struct Evidence {
    info: Info,
    threads: Vec<ThreadEvidence>,
    handles: Vec<HandleSnapshot>,
    memory: Option<FallibleArc<RetainedMemory>>,
    captured: bool,
    fault_recorded: bool,
    metadata_charge: Option<CommittedCharge>,
    memory_charge: Option<CommittedCharge>,
}

pub(super) struct Retention {
    evidence: Lock<Evidence>,
    release_reason: AtomicU32,
    deadline: AtomicU64,
    timeout_ns: u64,
    budget: BudgetOwner,
    signals: SignalState,
    _charge: CommittedCharge,
}
impl Retention {
    pub(super) fn new(
        owner: &Process,
        max_bytes: u64,
        timeout_ns: u64,
    ) -> Result<FallibleArc<Self>, Error> {
        if !policy::valid_request(max_bytes, timeout_ns) {
            return Err(Error::Invalid);
        }
        let budget = owner.retention_budget()?;
        if !budget.state.with(policy::Budget::arm) {
            return Err(Error::Limit);
        }
        let budget = BudgetOwner {
            budget,
            armed: AtomicBool::new(true),
        };
        let allocation = charge(
            &owner.resource_domain(),
            FallibleArc::<Self>::allocation_size(),
        )?;
        let bytes = MAX_THREADS * core::mem::size_of::<ThreadEvidence>()
            + MAX_HANDLES * core::mem::size_of::<HandleSnapshot>();
        let metadata = charge(&owner.resource_domain(), bytes)?;
        let memory_charge = charge(
            &owner.resource_domain(),
            FallibleArc::<RetainedMemory>::allocation_size(),
        )?;
        let mut threads = Vec::new();
        let mut handles = Vec::new();
        threads
            .try_reserve_exact(MAX_THREADS)
            .map_err(|_| Error::Process(ProcessError::Allocation))?;
        handles
            .try_reserve_exact(MAX_HANDLES)
            .map_err(|_| Error::Process(ProcessError::Allocation))?;
        FallibleArc::try_new(Self {
            evidence: Lock::new(Evidence {
                info: Info {
                    phase: 0,
                    flags: 0,
                    release_reason: 0,
                    process_koid: 0,
                    deadline_ns: 0,
                    retained_bytes: 0,
                    memory_limit: max_bytes,
                    terminal: None,
                    fault_pc: 0,
                    fault_address: 0,
                    fault_syndrome: 0,
                    thread_count: 0,
                    mapping_count: 0,
                    handle_count: 0,
                },
                threads,
                handles,
                memory: None,
                captured: false,
                fault_recorded: false,
                metadata_charge: Some(metadata),
                memory_charge: Some(memory_charge),
            }),
            release_reason: AtomicU32::new(0),
            deadline: AtomicU64::new(0),
            timeout_ns,
            budget,
            signals: SignalState::new(),
            _charge: allocation,
        })
        .map_err(|_| Error::Process(ProcessError::Allocation))
    }
    pub(super) fn started(&self, koid: u64) {
        self.evidence.with(|state| {
            state.info.phase = 1;
            state.info.process_koid = koid;
        });
    }
    pub(super) fn stopping(&self, terminal: TerminalReason) {
        let now = now_ns();
        let deadline = now.saturating_add(self.timeout_ns);
        let _ = self
            .deadline
            .compare_exchange(0, deadline, Ordering::AcqRel, Ordering::Acquire);
        self.evidence.with(|state| {
            if state.info.terminal.is_none() {
                state.info.terminal = Some(terminal);
            }
            state.info.phase = 2;
            state.info.deadline_ns = self.deadline.load(Ordering::Acquire);
        });
    }
    pub(super) fn fault(&self, pc: u64, address: u64, syndrome: u64) {
        self.evidence.with(|state| {
            if state.info.terminal.is_none() && !state.fault_recorded {
                state.fault_recorded = true;
                state.info.fault_pc = pc;
                state.info.fault_address = address;
                state.info.fault_syndrome = syndrome;
            }
        });
    }
    pub(super) fn thread(&self, koid: u64, context: &crate::hal::user::UserContext) {
        // Called while Process owns its lifecycle lock. Deadline expiry and
        // wakeups belong to the reaper; this path only copies scalar evidence.
        if self.release_reason.load(Ordering::Acquire) != 0 {
            return;
        }
        self.evidence.with(|state| {
            if self.release_reason.load(Ordering::Acquire) != 0 {
                return;
            }
            if state.threads.len() == MAX_THREADS {
                state.info.flags |= 2;
                return;
            }
            state.threads.push(ThreadEvidence {
                koid,
                registers: UserRegisterSnapshot::EMPTY,
            });
            if let Some(thread) = state.threads.last_mut() {
                context.snapshot_registers(&mut thread.registers);
            }
            state.info.thread_count = state.threads.len();
        });
    }
    pub(super) fn handle(&self, handle: HandleSnapshot) {
        if self.released() {
            return;
        }
        self.evidence.with(|state| {
            if self.release_reason.load(Ordering::Acquire) != 0 {
                return;
            }
            if state.handles.len() == MAX_HANDLES {
                state.info.flags |= 4;
                return;
            }
            state.handles.push(handle);
            state.info.handle_count = state.handles.len();
        });
    }
    /// Called by the process reaper after execution and handle withdrawal. Page
    /// selection is local and bounded; it never follows a file refill source.
    pub(super) fn capture(&self, address: &NativeAddressSpace) {
        if self.evidence.with(|state| state.captured) {
            return;
        }
        let requested = self.evidence.with(|state| state.info.memory_limit);
        let admitted = if self.released() {
            None
        } else {
            self.budget
                .budget
                .state
                .with(|budget| budget.retain(requested))
        };
        let memory = if let Some(bytes) = admitted {
            // SAFETY: Process::retire calls this only after every thread has
            // detached, transactions have drained, and all active handles were
            // withdrawn. The address space is sealed against external VMARs.
            unsafe { address.capture_crash_memory(bytes) }.ok()
        } else {
            None
        };
        let retained = admitted.map(|reserved| {
            let bytes = memory.as_ref().map_or(0, |memory| memory.retained_bytes());
            self.budget
                .budget
                .state
                .with(|budget| budget.commit_bytes(reserved, bytes));
            RetainedBudget {
                budget: self.budget.budget.clone(),
                bytes,
            }
        });
        let flags = memory.as_ref().map(|memory| {
            (
                memory.retained_bytes(),
                memory.mapping_count(),
                memory.partial(),
                memory.mappings_partial(),
            )
        });
        let mut payload = retained.and_then(|budget| {
            let allocation = self.evidence.with(|state| state.memory_charge.take())?;
            FallibleArc::try_new(RetainedMemory {
                memory,
                _budget: budget,
                _charge: allocation,
            })
            .ok()
        });
        self.evidence.with(|state| {
            state.captured = true;
            if self.release_reason.load(Ordering::Acquire) != 0 {
                return;
            }
            state.info.phase = 3;
            if let Some((bytes, mappings, partial, mappings_partial)) =
                flags.filter(|_| payload.is_some())
            {
                state.info.retained_bytes = bytes;
                state.info.mapping_count = mappings;
                if partial {
                    state.info.flags |= 1;
                }
                if mappings_partial {
                    state.info.flags |= 8;
                }
            } else {
                state.info.flags |= 1 | 8;
            }
            state.memory = payload.take();
        });
        // A release racing capture does not publish new retained resources.
        // Destroy any rejected payload outside the evidence lock.
        drop(payload);
        self.signal(READY);
    }
    pub(super) fn release(&self, reason: u32) {
        let changed = self.evidence.with(|_| {
            self.release_reason
                .compare_exchange(0, reason, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        });
        if changed {
            // Revoked views must not occupy a future registration slot. This
            // counters-only step also runs in the zero-active-handle callback.
            self.budget.disarm();
            self.signal(RELEASED);
            crate::reaper::request();
        }
    }
    pub(super) fn released(&self) -> bool {
        let deadline = self.deadline.load(Ordering::Acquire);
        if deadline != 0 && now_ns() >= deadline {
            self.release(RELEASE_DEADLINE);
        }
        self.release_reason.load(Ordering::Acquire) != 0
    }
    pub(super) fn discard_released(&self) {
        let retired = self.evidence.with(|state| {
            (
                state.memory.take(),
                core::mem::take(&mut state.threads),
                core::mem::take(&mut state.handles),
                state.metadata_charge.take(),
                state.memory_charge.take(),
            )
        });
        drop(retired);
    }
    fn signal(&self, bits: SignalMask) {
        if self.signals.update(SignalMask::EMPTY, bits).is_err() {
            hyper::debug::invariant_failure("process retention signal invariant");
        }
    }
    fn info(&self) -> Info {
        let released = self.released();
        let mut info = self.evidence.with(|state| state.info);
        if released {
            info.phase = 4;
            info.release_reason = self.release_reason.load(Ordering::Acquire);
        }
        info
    }
    fn check_view(&self) -> Result<(), Error> {
        self.observe(|_| Ok(()))
    }
    fn observe<T>(&self, read: impl FnOnce(&Evidence) -> Result<T, Error>) -> Result<T, Error> {
        if self.released() {
            return Err(Error::Revoked);
        }
        self.evidence.with(|state| {
            if self.release_reason.load(Ordering::Acquire) != 0 {
                return Err(Error::Revoked);
            }
            if !state.captured {
                return Err(Error::Busy);
            }
            read(state)
        })
    }
}

pub(crate) struct ProcessRetirement {
    pub(super) retention: FallibleArc<Retention>,
    _charge: CommittedCharge,
}
impl ProcessRetirement {
    pub(crate) const RIGHTS: Rights = Rights::TRANSFER
        .union(Rights::WAIT)
        .union(Rights::INSPECT)
        .union(Rights::INSPECT_DETAILS)
        .union(Rights::RETIRE);
    pub(super) fn new(
        retention: FallibleArc<Retention>,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        Ok(Self {
            retention,
            _charge: object_charge(
                sponsor,
                object_allocation_size::<Self>().ok_or(Error::Invalid)?,
            )?,
        })
    }
    pub(crate) fn info(&self) -> Info {
        self.retention.info()
    }
    pub(crate) fn release(&self) {
        self.retention.release(RELEASE_EXPLICIT);
    }
    pub(crate) fn open_view(&self, sponsor: &ResourceDomain) -> Result<CrashView, Error> {
        self.retention.check_view()?;
        Ok(CrashView {
            retention: self.retention.clone(),
            _charge: object_charge(
                sponsor,
                object_allocation_size::<CrashView>().ok_or(Error::Invalid)?,
            )?,
        })
    }
}
impl Drop for ProcessRetirement {
    fn drop(&mut self) {
        // Also covers a failed object publication, before any active handle
        // existed to invoke on_zero_active_handles.
        self.retention.release(RELEASE_OWNER_LOST);
    }
}
impl private::Sealed for ProcessRetirement {}
impl private::UserExportable for ProcessRetirement {}
impl KernelObject for ProcessRetirement {
    const KIND: ObjectKind = ObjectKind::PROCESS_RETIREMENT;
    const SUPPORTED_RIGHTS: Rights = Self::RIGHTS;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(SignalSource::new(
            &self.retention.signals,
            READY.union(RELEASED),
        ))
    }
    fn on_zero_active_handles(&self, _: &mut crate::object::ObjectRetirement) {
        self.retention.release(RELEASE_OWNER_LOST);
    }
}

pub(crate) struct CrashView {
    retention: FallibleArc<Retention>,
    _charge: CommittedCharge,
}
impl CrashView {
    pub(crate) const RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::READ);
    pub(crate) fn info(&self) -> Result<Info, Error> {
        self.retention.observe(|state| Ok(state.info))
    }
    pub(crate) fn thread(&self, index: usize) -> Result<(ThreadEvidence, usize), Error> {
        self.retention.observe(|state| {
            let evidence = *state.threads.get(index).ok_or(Error::NotFound)?;
            Ok((
                evidence,
                if index + 1 < state.threads.len() {
                    index + 1
                } else {
                    0
                },
            ))
        })
    }
    pub(crate) fn handle(&self, index: usize) -> Result<(HandleSnapshot, usize), Error> {
        self.retention.observe(|state| {
            let evidence = *state.handles.get(index).ok_or(Error::NotFound)?;
            Ok((
                evidence,
                if index + 1 < state.handles.len() {
                    index + 1
                } else {
                    0
                },
            ))
        })
    }
    fn memory(&self) -> Result<FallibleArc<RetainedMemory>, Error> {
        self.retention
            .observe(|state| state.memory.clone().ok_or(Error::NotFound))
    }
    pub(crate) fn mapping(&self, index: usize) -> Result<(CrashMapping, usize), Error> {
        let memory = self.memory()?;
        let memory = memory.memory.as_ref().ok_or(Error::NotFound)?;
        let mapping = memory.mapping(index).ok_or(Error::NotFound)?;
        Ok((
            mapping,
            if index + 1 < memory.mapping_count() {
                index + 1
            } else {
                0
            },
        ))
    }
    pub(crate) fn read(&self, address: u64, output: &mut [u8]) -> Result<usize, Error> {
        if output.len() > 65536 {
            return Err(Error::Invalid);
        }
        self.memory()?
            .memory
            .as_ref()
            .ok_or(Error::NotFound)?
            .read(address, output)
            .map_err(|error| match error {
                CrashReadError::InvalidRange => Error::Invalid,
                CrashReadError::Unavailable | CrashReadError::Backing => Error::NotFound,
            })
    }
}
impl private::Sealed for CrashView {}
impl private::UserExportable for CrashView {}
impl KernelObject for CrashView {
    const KIND: ObjectKind = ObjectKind::CRASH_VIEW;
    const SUPPORTED_RIGHTS: Rights = Self::RIGHTS;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
}

fn charge(sponsor: &ResourceDomain, bytes: usize) -> Result<CommittedCharge, Error> {
    sponsor
        .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, bytes as u64))
        .map(|reservation| reservation.commit())
        .map_err(|error| Error::Process(ProcessError::Resource(error)))
}
fn object_charge(sponsor: &ResourceDomain, bytes: usize) -> Result<CommittedCharge, Error> {
    sponsor
        .reserve(
            ResourceAmount::ZERO
                .with(ResourceKind::KernelMemoryBytes, bytes as u64)
                .with(ResourceKind::KernelObjects, 1),
        )
        .map(|reservation| reservation.commit())
        .map_err(|error| Error::Process(ProcessError::Resource(error)))
}
fn now_ns() -> u64 {
    crate::time::monotonic_nanoseconds().unwrap_or(u64::MAX)
}
