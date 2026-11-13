// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Atomic waits keyed by private mapping lineage or explicitly retained VMO identity.

use hyper::sync::InterruptSpinLock;
#[derive(Debug)]
pub(crate) enum Error {
    Process(ProcessError),
    Wait(crate::object::ObjectWaitError),
    InvalidInput,
}
impl From<ProcessError> for Error {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}
impl From<crate::object::ObjectWaitError> for Error {
    fn from(error: crate::object::ObjectWaitError) -> Self {
        Self::Wait(error)
    }
}
use super::{Process, ProcessError};
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceKind,
};
use {
    crate::mm::user_space::AtomicWaitKey, crate::mm::user_space::NativePinnedAtomicWord,
    crate::mm::user_space::UserAddress,
};
use {crate::object::TimedWaitPreparation, crate::object::prepare_timed_wait};
use {
    crate::task::WaitOutcome, crate::task::WaitQueue, crate::task::WaitTicket,
    crate::task::scheduler,
};

type Bucket = InterruptSpinLock<State, crate::hal::irq::LocalMask>;
struct State {
    queue: WaitQueue,
    waiters: Option<WaitTicket>,
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    scanned_waiters: u64,
}

/// Private and shared namespaces cannot alias even when they use the same page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Key {
    Private(AtomicWaitKey),
    Shared {
        vmo: crate::object::Koid,
        offset: u64,
    },
}
impl From<AtomicWaitKey> for Key {
    fn from(value: AtomicWaitKey) -> Self {
        Self::Private(value)
    }
}
impl Key {
    fn hash(self) -> u64 {
        match self {
            Self::Private(key) => key.hash(),
            Self::Shared { vmo, offset } => {
                fn mix(mut value: u64) -> u64 {
                    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                    value ^ (value >> 31)
                }
                mix(vmo.get()) ^ mix(offset) ^ 1
            }
        }
    }
}

/// Shared waits retain both storage admission and the object whose KOID is used
/// as the namespace. A backing-only owner would allow that identity to retire.
pub(crate) struct SharedWord {
    word: crate::mm::user_space::NativeSharedAtomicWord,
    object: crate::object::KernelRef<crate::mm::user_space::VmoObject, crate::object::OperationPin>,
    offset: u64,
    _pin_charge: CommittedCharge,
}
impl SharedWord {
    /// Admit caller pin quota before touching source backing; source page
    /// allocation remains charged to the VMO's own resource account.
    pub(crate) fn prepare(
        object: crate::object::KernelRef<
            crate::mm::user_space::VmoObject,
            crate::object::OperationPin,
        >,
        offset: u64,
        domain: &crate::accounting::ResourceDomain,
    ) -> Result<Self, crate::mm::user_space::MemoryObjectError> {
        object.object().validate_shared_atomic_offset(offset)?;
        let pin_charge = domain
            .reserve(ResourceAmount::ZERO.with(ResourceKind::PinnedPages, 1))?
            .commit();
        let word = object.object().pin_shared_atomic_u32(offset)?;
        Ok(Self {
            word,
            object,
            offset,
            _pin_charge: pin_charge,
        })
    }
}

enum RetainedWord {
    Private(NativePinnedAtomicWord),
    Shared(SharedWord),
}

impl RetainedWord {
    fn key(&self) -> Key {
        match self {
            Self::Private(word) => Key::Private(word.key),
            Self::Shared(word) => Key::Shared {
                vmo: word.object.koid(),
                offset: word.offset,
            },
        }
    }
    fn physical(&self) -> u64 {
        match self {
            Self::Private(word) => word.physical,
            Self::Shared(word) => word.word.physical(),
        }
    }
}

/// One condition registration owned by the scheduler Thread, including its
/// backing lease and resource admission. Bucket links are generation-qualified
/// identities, never pointers into a suspended continuation.
struct Waiter {
    word: RetainedWord,
    ticket: WaitTicket,
    next: Option<WaitTicket>,
    _charge: CommittedCharge,
}

#[derive(Clone, Copy)]
struct WaiterSnapshot {
    key: Key,
    next: Option<WaitTicket>,
}

/// Embedded once in each Thread. Registry protection retains the owner during
/// access; this lock protects the slot independently of CPU schedule ownership.
/// Lock order is bucket -> registry reader -> slot. Slot operations never call
/// the scheduler or acquire a bucket, and return owners for destruction outside
/// all three locks.
pub(crate) struct ThreadWaiter {
    node: InterruptSpinLock<Option<Waiter>, crate::hal::irq::LocalMask>,
}

impl ThreadWaiter {
    pub(crate) const fn new() -> Self {
        Self {
            node: InterruptSpinLock::new(None),
        }
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.node.with(|node| node.is_none())
    }

    fn install(&self, waiter: Waiter) {
        self.node.with(|node| {
            if node.is_some() {
                invariant();
            }
            *node = Some(waiter);
        });
    }

    fn snapshot(&self, ticket: WaitTicket) -> WaiterSnapshot {
        self.node.with(|node| {
            let waiter = Self::matching(node, ticket);
            WaiterSnapshot {
                key: waiter.word.key(),
                next: waiter.next,
            }
        })
    }

    fn set_next(&self, ticket: WaitTicket, next: Option<WaitTicket>) {
        self.node
            .with(|node| Self::matching(node, ticket).next = next);
    }

    fn remove(&self, ticket: WaitTicket) -> Waiter {
        self.node.with(|node| {
            Self::matching(node, ticket);
            match node.take() {
                Some(waiter) => waiter,
                None => invariant(),
            }
        })
    }

    fn matching(node: &mut Option<Waiter>, ticket: WaitTicket) -> &mut Waiter {
        match node {
            Some(waiter) if waiter.ticket == ticket => waiter,
            _ => invariant(),
        }
    }
}

impl Drop for ThreadWaiter {
    fn drop(&mut self) {
        if !self.is_idle() {
            hyper::debug::invariant_failure(
                "atomic wait Thread destroyed with linked registration",
            );
        }
    }
}

fn with_waiter<R>(ticket: WaitTicket, operation: impl FnOnce(&ThreadWaiter) -> R) -> R {
    match scheduler::with_wait_context(ticket.thread(), |context| operation(&context.atomic)) {
        Ok(value) => value,
        Err(_) => invariant(),
    }
}

struct Registration {
    ticket: WaitTicket,
    bucket: &'static Bucket,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let waiter = self.bucket.with(|state| state.unlink(self.ticket));
        // Mapping owners and quota charges must be released outside IRQ locks.
        drop(waiter);
    }
}

impl State {
    fn unlink(&mut self, target: WaitTicket) -> Waiter {
        let mut current = self.waiters;
        let mut previous = None;
        while let Some(ticket) = current {
            let next = with_waiter(ticket, |slot| slot.snapshot(ticket)).next;
            if ticket == target {
                if let Some(previous) = previous {
                    with_waiter(previous, |slot| slot.set_next(previous, next));
                } else {
                    self.waiters = next;
                }
                return with_waiter(ticket, |slot| slot.remove(ticket));
            }
            previous = Some(ticket);
            current = next;
        }
        invariant()
    }
}

const BUCKET_COUNT: usize = 64;
static BUCKETS: [Bucket; BUCKET_COUNT] = [const {
    Bucket::new(State {
        queue: WaitQueue::new(),
        waiters: None,
        #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
        scanned_waiters: 0,
    })
}; BUCKET_COUNT];
fn bucket(key: impl Into<Key>) -> &'static Bucket {
    &BUCKETS[key.into().hash() as usize % BUCKET_COUNT]
}

pub(crate) fn wait(
    process: &Process,
    address: u64,
    expected: u32,
    deadline: u64,
    cancelled: impl Fn() -> bool,
) -> Result<WaitOutcome, Error> {
    if !address.is_multiple_of(4) {
        return Err(Error::InvalidInput);
    }
    let word =
        process.retry_user_memory(|space| space.pin_atomic_u32(UserAddress::new(address)))?;
    wait_word(
        process,
        RetainedWord::Private(word),
        expected,
        deadline,
        cancelled,
    )
}

pub(crate) fn wait_shared(
    process: &Process,
    word: SharedWord,
    expected: u32,
    deadline: u64,
    cancelled: impl Fn() -> bool,
) -> Result<WaitOutcome, Error> {
    wait_word(
        process,
        RetainedWord::Shared(word),
        expected,
        deadline,
        cancelled,
    )
}

fn wait_word(
    process: &Process,
    word: RetainedWord,
    expected: u32,
    deadline: u64,
    cancelled: impl Fn() -> bool,
) -> Result<WaitOutcome, Error> {
    let linear = crate::mm::memory::linear_address(word.physical()).ok_or(Error::InvalidInput)?;
    // SAFETY: aligned u32 is wholly within a resident writable page. `word`
    // retains mapping/storage admission through unlink, independently of unmap
    // and handle closure. All accesses here are atomic, never byte copies.
    let load = || unsafe {
        crate::hal::atomic::load_external_u32(core::ptr::with_exposed_provenance(linear))
    };
    if load() != expected {
        return Ok(WaitOutcome::Notified);
    }
    let domain = process.resource_domain();
    let charge = domain
        .reserve(ResourceAmount::ZERO.with(
            ResourceKind::KernelMemoryBytes,
            core::mem::size_of::<Waiter>() as u64,
        ))
        .map_err(ProcessError::from)?
        .commit();
    // Preserve per-wait resource admission without allocating a condition node.
    // Storage belongs to the Thread from its construction until retirement.
    let prepared = match prepare_timed_wait(&domain, deadline)? {
        TimedWaitPreparation::Completed(outcome) => return Ok(outcome),
        TimedWaitPreparation::Armed(prepared) => prepared,
    };
    let ticket = prepared.ticket();
    let bucket = bucket(word.key());
    let mut candidate = Some(Waiter {
        word,
        ticket,
        next: None,
        _charge: charge,
    });
    enum Publication {
        Mismatch(crate::object::PreparedTimedWait),
        Waiting(crate::object::PublishedTimedWait, Registration),
    }
    // SAFETY: condition check, registration and wake all use this bucket lock.
    // The retained IRQ mask goes directly to finish or is dropped on mismatch.
    let (published, mask) = unsafe {
        bucket.with_mask_retained(|state| {
            if load() != expected {
                return Publication::Mismatch(prepared);
            }
            if cancelled() {
                prepared.request_cancellation();
            }
            let mut waiter = match candidate.take() {
                Some(waiter) => waiter,
                None => invariant(),
            };
            waiter.next = state.waiters;
            with_waiter(ticket, |slot| slot.install(waiter));
            state.waiters = Some(ticket);
            let registration = Registration { ticket, bucket };
            Publication::Waiting(prepared.publish_locked(&state.queue), registration)
        })
    };
    match published {
        Publication::Mismatch(prepared) => {
            drop(mask);
            prepared.abort()?;
            drop(candidate);
            Ok(WaitOutcome::Notified)
        }
        Publication::Waiting(published, registration) => {
            let (outcome, retirement) = published.finish(mask);
            drop(registration);
            retirement?;
            Ok(outcome)
        }
    }
}

pub(crate) fn wake(process: &Process, address: u64, count: u32) -> Result<u64, Error> {
    if !address.is_multiple_of(4) {
        return Err(Error::InvalidInput);
    }
    let key =
        process.retry_user_memory(|space| space.atomic_wait_key(UserAddress::new(address)))?;
    wake_key(Key::Private(key), count)
}

pub(crate) fn wake_shared(vmo: crate::object::Koid, offset: u64, count: u32) -> Result<u64, Error> {
    wake_key(Key::Shared { vmo, offset }, count)
}

fn wake_key(key: Key, count: u32) -> Result<u64, Error> {
    bucket(key).with(|state| {
        let mut woke = 0;
        let mut current = state.waiters;
        while let Some(ticket) = current {
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            {
                state.scanned_waiters = state.scanned_waiters.wrapping_add(1);
            }
            let waiter = with_waiter(ticket, |slot| slot.snapshot(ticket));
            current = waiter.next;
            if woke == u64::from(count) {
                break;
            }
            if waiter.key == key
                && scheduler::notify_registered_with(ticket, || {})
                    .map_err(ProcessError::from)?
                    .won
            {
                woke += 1;
            }
        }
        Ok(woke)
    })
}

fn invariant() -> ! {
    crate::diagnostics::crash::fatal(format_args!("HypeR: atomic wait lost registration"))
}

#[cfg(all(
    feature = "kernel-test",
    feature = "kernel-self-test",
    CONFIG_ARCH_AARCH64
))]
pub(crate) fn bucket_index_for_test(process: &Process, address: u64) -> Result<usize, Error> {
    let key =
        process.retry_user_memory(|space| space.atomic_wait_key(UserAddress::new(address)))?;
    Ok(key.hash() as usize % BUCKET_COUNT)
}

#[cfg(all(
    feature = "kernel-test",
    feature = "kernel-self-test",
    CONFIG_ARCH_AARCH64
))]
pub(crate) fn scanned_waiters_for_test() -> u64 {
    BUCKETS.iter().fold(0u64, |total, bucket| {
        total.wrapping_add(bucket.with(|state| state.scanned_waiters))
    })
}

pub(crate) fn sleep(
    process: &Process,
    deadline: u64,
    cancelled: impl Fn() -> bool,
) -> Result<WaitOutcome, Error> {
    let prepared = match prepare_timed_wait(&process.resource_domain(), deadline)? {
        TimedWaitPreparation::Completed(outcome) => return Ok(outcome),
        TimedWaitPreparation::Armed(prepared) => prepared,
    };
    if cancelled() {
        prepared.request_cancellation();
    }
    // SAFETY: the bucket is permanent; its queue is only used for exact-ticket
    // resolution. No ordinary wake operation targets sleepers on this queue.
    let (published, mask) =
        unsafe { BUCKETS[0].with_mask_retained(|state| prepared.publish_locked(&state.queue)) };
    let (outcome, retirement) = published.finish(mask);
    retirement?;
    Ok(outcome)
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
#[cfg_attr(
    all(feature = "kernel-test", feature = "kernel-self-test"),
    allow(
        dead_code,
        reason = "Used by the AArch64 Native self-tests; other HAL self-tests exercise different entry paths"
    )
)]
pub(crate) fn waiter_count(process: &Process, address: u64) -> Result<usize, Error> {
    let word =
        process.retry_user_memory(|space| space.pin_atomic_u32(UserAddress::new(address)))?;
    Ok(bucket(word.key).with(|state| {
        let mut count = 0;
        let mut current = state.waiters;
        while let Some(ticket) = current {
            let waiter = with_waiter(ticket, |slot| slot.snapshot(ticket));
            current = waiter.next;
            if waiter.key == Key::Private(word.key) {
                count += 1;
            }
        }
        count
    }))
}

/// Exercises the interval after scheduler completion but before condition
/// unlink: an idle arbitration record alone must not authorize Thread exit.
#[cfg(all(
    all(feature = "kernel-test", feature = "kernel-self-test"),
    CONFIG_ARCH_AARCH64
))]
pub(crate) fn verify_exit_guard(process: &Process, address: u64) -> Result<bool, Error> {
    let word =
        process.retry_user_memory(|space| space.pin_atomic_u32(UserAddress::new(address)))?;
    let domain = process.resource_domain();
    let charge = domain
        .reserve(ResourceAmount::ZERO.with(
            ResourceKind::KernelMemoryBytes,
            core::mem::size_of::<Waiter>() as u64,
        ))
        .map_err(ProcessError::from)?
        .commit();
    let registration =
        scheduler::begin_wait(crate::task::WaitMobility::Migratable).map_err(ProcessError::from)?;
    let ticket = registration.ticket();
    let _ = scheduler::finish_wait(registration).map_err(ProcessError::from)?;
    let bucket = bucket(word.key);
    bucket.with(|state| {
        with_waiter(ticket, |slot| {
            slot.install(Waiter {
                word: RetainedWord::Private(word),
                ticket,
                next: state.waiters,
                _charge: charge,
            })
        });
        state.waiters = Some(ticket);
    });
    let guard = Registration { ticket, bucket };
    let rejected = scheduler::verify_wait_exit_rejected();
    drop(guard);
    rejected.map_err(ProcessError::from).map_err(Error::from)
}
