// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Interrupt masking and explicitly ordered atomic-operation contracts.

use core::sync::atomic::{AtomicUsize, Ordering};

use hyper::hal::interrupt::InterruptMask;
use hyper::sync::atomic::{AtomicFlag, AtomicU64, Ordering as AtomicOrdering, fence};
use hyper::sync::{
    AtomicBorrowClaim, AtomicBorrowError, AtomicBorrowPtr, GenerationTaggedState,
    InterruptMaskGuard, InterruptSpinLock, PublishedOnce,
};
use std::sync::{Arc, Barrier};
use std::thread;

static MASK_DEPTH: AtomicUsize = AtomicUsize::new(0);
static CONTENTION_WAITS: AtomicUsize = AtomicUsize::new(0);

struct TestInterruptMask;

impl InterruptMask for TestInterruptMask {
    type State = usize;

    fn save_and_disable() -> Self::State {
        MASK_DEPTH.fetch_add(1, Ordering::SeqCst)
    }

    fn restore(state: Self::State) {
        MASK_DEPTH.store(state, Ordering::SeqCst);
    }
}

struct ProgressInterruptMask;

impl InterruptMask for ProgressInterruptMask {
    type State = ();

    fn save_and_disable() -> Self::State {}

    fn restore(_: Self::State) {}

    fn wait_for_lock_owner() {
        CONTENTION_WAITS.fetch_add(1, Ordering::SeqCst);
        thread::yield_now();
    }
}

// Inference is unique only while InterruptMaskGuard implements neither auto
// trait. If it becomes Send or Sync, the corresponding conditional impl makes
// this marker selection ambiguous and compilation fails.
const _: fn() = || {
    trait AmbiguousIfImpl<Marker: ?Sized> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfImpl<dyn Send> for T {}
    impl<T: ?Sized + Sync> AmbiguousIfImpl<dyn Sync> for T {}

    let _ = <InterruptMaskGuard<TestInterruptMask> as AmbiguousIfImpl<_>>::marker;
};

#[repr(align(2))]
struct AlignedByte(u8);

const _: fn() = || {
    trait AmbiguousIfImpl<Marker: ?Sized> {
        fn marker() {}
    }

    impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfImpl<dyn Send> for T {}
    impl<T: ?Sized + Sync> AmbiguousIfImpl<dyn Sync> for T {}

    let _ = <AtomicBorrowClaim<'static, AlignedByte> as AmbiguousIfImpl<_>>::marker;
};

#[test]
fn atomic_borrow_pointer_enforces_exact_state_transitions() {
    let state = AtomicBorrowPtr::new();
    let mut value = AlignedByte(7);
    let pointer = core::ptr::NonNull::from(&mut value);

    assert!(matches!(state.begin_borrow(), Ok(None)));
    assert_eq!(state.publish(pointer), Ok(()));
    assert_eq!(state.publish(pointer), Err(AtomicBorrowError::Active));
    let claim = match state.begin_borrow() {
        Ok(Some(claim)) => claim,
        Ok(None) | Err(_) => panic!("active pointer was not claimable"),
    };
    assert_eq!(claim.pointer(), pointer);
    assert!(matches!(
        state.begin_borrow(),
        Err(AtomicBorrowError::Borrowed)
    ));
    assert_eq!(state.unpublish(pointer), Err(AtomicBorrowError::Borrowed));
    assert_eq!(claim.finish(), Ok(()));
    assert_eq!(state.unpublish(pointer), Ok(()));
    assert_eq!(state.unpublish(pointer), Err(AtomicBorrowError::Inactive));
    assert_eq!(value.0, 7);
}

#[test]
fn atomic_borrow_pointer_rejects_mismatch_without_changing_owner() {
    let state = AtomicBorrowPtr::new();
    let mut owner = AlignedByte(1);
    let mut stranger = AlignedByte(2);
    let owner = core::ptr::NonNull::from(&mut owner);
    let stranger = core::ptr::NonNull::from(&mut stranger);

    assert_eq!(state.publish(owner), Ok(()));
    assert_eq!(
        state.unpublish(stranger),
        Err(AtomicBorrowError::PointerMismatch)
    );
    let claim = match state.begin_borrow() {
        Ok(Some(claim)) => claim,
        Ok(None) | Err(_) => panic!("owner pointer was not claimable"),
    };
    assert_eq!(claim.pointer(), owner);
    assert_eq!(claim.finish(), Ok(()));
    assert_eq!(state.unpublish(owner), Ok(()));
}

#[test]
fn forgotten_atomic_borrow_claim_leaves_state_safely_borrowed() {
    let state = AtomicBorrowPtr::new();
    let mut value = AlignedByte(3);
    let pointer = core::ptr::NonNull::from(&mut value);

    assert_eq!(state.publish(pointer), Ok(()));
    let claim = match state.begin_borrow() {
        Ok(Some(claim)) => claim,
        Ok(None) | Err(_) => panic!("active pointer was not claimable"),
    };
    core::mem::forget(claim);
    assert!(matches!(
        state.begin_borrow(),
        Err(AtomicBorrowError::Borrowed)
    ));
    assert_eq!(state.unpublish(pointer), Err(AtomicBorrowError::Borrowed));
}

#[test]
fn dropped_atomic_borrow_claim_restores_active_state() {
    let state = AtomicBorrowPtr::new();
    let mut value = AlignedByte(6);
    let pointer = core::ptr::NonNull::from(&mut value);

    assert_eq!(state.publish(pointer), Ok(()));
    let claim = match state.begin_borrow() {
        Ok(Some(claim)) => claim,
        Ok(None) | Err(_) => panic!("active pointer was not claimable"),
    };
    drop(claim);
    let claim = match state.begin_borrow() {
        Ok(Some(claim)) => claim,
        Ok(None) | Err(_) => panic!("dropped claim did not restore active state"),
    };
    assert_eq!(claim.pointer(), pointer);
    assert_eq!(claim.finish(), Ok(()));
    assert_eq!(state.unpublish(pointer), Ok(()));
}

#[test]
fn atomic_borrow_pointer_rejects_low_bit_pointer() {
    let state = AtomicBorrowPtr::new();
    let mut value = AlignedByte(4);
    let pointer = core::ptr::NonNull::from(&mut value);
    let tagged = pointer.as_ptr().map_addr(|address| address | 1);
    let Some(tagged) = core::ptr::NonNull::new(tagged) else {
        panic!("tagging a non-null pointer produced null");
    };

    assert_eq!(
        state.publish(tagged),
        Err(AtomicBorrowError::InvalidPointer)
    );
    assert!(matches!(state.begin_borrow(), Ok(None)));
}

#[test]
fn concurrent_atomic_borrow_has_one_claim_winner() {
    let state = Arc::new(AtomicBorrowPtr::new());
    let start = Arc::new(Barrier::new(2));
    let attempted = Arc::new(Barrier::new(2));
    let winners = Arc::new(AtomicUsize::new(0));
    let losers = Arc::new(AtomicUsize::new(0));
    let mut value = AlignedByte(5);
    let pointer = core::ptr::NonNull::from(&mut value);
    assert_eq!(state.publish(pointer), Ok(()));

    thread::scope(|scope| {
        for _ in 0..2 {
            let state = Arc::clone(&state);
            let start = Arc::clone(&start);
            let attempted = Arc::clone(&attempted);
            let winners = Arc::clone(&winners);
            let losers = Arc::clone(&losers);
            scope.spawn(move || {
                start.wait();
                let claim = state.begin_borrow();
                // Keep the winner Borrowed until both contenders have made
                // their one attempt; no scheduling assumption is needed.
                attempted.wait();
                match claim {
                    Ok(Some(claim)) => {
                        winners.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(claim.finish(), Ok(()));
                    }
                    Err(AtomicBorrowError::Borrowed) => {
                        losers.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(None) | Err(_) => panic!("unexpected concurrent claim result"),
                }
            });
        }
    });

    assert_eq!(winners.load(Ordering::SeqCst), 1);
    assert_eq!(losers.load(Ordering::SeqCst), 1);
    assert_eq!(state.unpublish(pointer), Ok(()));
}

#[test]
fn interrupt_lock_restores_the_previous_mask_state() {
    let lock = InterruptSpinLock::<_, TestInterruptMask>::new(41usize);
    lock.with(|value| {
        assert_eq!(MASK_DEPTH.load(Ordering::SeqCst), 1);
        *value += 1;
        assert!(lock.try_with(|_| ()).is_none());
        assert_eq!(MASK_DEPTH.load(Ordering::SeqCst), 1);
    });
    assert_eq!(MASK_DEPTH.load(Ordering::SeqCst), 0);
    assert_eq!(lock.try_with(|value| *value), Some(42));
    assert_eq!(MASK_DEPTH.load(Ordering::SeqCst), 0);
}

#[test]
fn both_interrupt_lock_acquisition_paths_run_architecture_progress() {
    exercise_contended_lock(false);
    exercise_contended_lock(true);
}

fn exercise_contended_lock(retain_mask: bool) {
    CONTENTION_WAITS.store(0, Ordering::SeqCst);
    let lock = Arc::new(InterruptSpinLock::<_, ProgressInterruptMask>::new(0usize));
    let held = Arc::new(core::sync::atomic::AtomicBool::new(false));

    let owner_lock = Arc::clone(&lock);
    let owner_held = Arc::clone(&held);
    let owner = thread::spawn(move || {
        owner_lock.with(|value| {
            owner_held.store(true, Ordering::Release);
            while CONTENTION_WAITS.load(Ordering::Acquire) == 0 {
                thread::yield_now();
            }
            *value = 41;
        });
    });

    while !held.load(Ordering::Acquire) {
        thread::yield_now();
    }
    let contender_lock = Arc::clone(&lock);
    let contender = thread::spawn(move || {
        if retain_mask {
            // SAFETY: The retained guard is dropped on this same host thread
            // immediately after acquisition, preserving CPU affinity and order.
            let ((), guard) = unsafe {
                contender_lock.with_mask_retained(|value| {
                    *value += 1;
                })
            };
            drop(guard);
        } else {
            contender_lock.with(|value| *value += 1);
        }
    });

    assert!(owner.join().is_ok());
    assert!(contender.join().is_ok());
    assert!(CONTENTION_WAITS.load(Ordering::Acquire) > 0);
    assert_eq!(lock.with(|value| *value), 42);
}

#[test]
fn interrupt_mask_default_lock_wait_is_available() {
    // TestInterruptMask intentionally does not override the progress hook. This
    // call is also a compile-time contract for the architecture-neutral default.
    TestInterruptMask::wait_for_lock_owner();
}

#[test]
fn atomic_flag_and_counter_use_explicit_ordering() {
    let flag = AtomicFlag::default();
    assert!(flag.try_acquire());
    assert!(!flag.try_acquire());
    assert!(flag.is_acquired());
    flag.release();
    assert!(!flag.is_acquired());

    let counter = AtomicU64::new(40);
    assert_eq!(counter.fetch_add(2, AtomicOrdering::AcqRel), 40);
    fence(AtomicOrdering::SeqCst);
    assert_eq!(counter.load(AtomicOrdering::Acquire), 42);
}

#[test]
fn delayed_generation_cannot_claim_a_republished_state() {
    const PENDING: u8 = 1;
    const WORKING: u8 = 2;

    let stale_pending = GenerationTaggedState::new(7, PENDING);
    let current_pending = GenerationTaggedState::new(8, PENDING);
    let slot = AtomicU64::new(stale_pending.bits());

    // Model a publisher completing generation 7 and reusing the slot for 8
    // before a delayed generation-7 observer reaches its atomic claim.
    slot.store(current_pending.bits(), AtomicOrdering::Release);
    assert!(
        slot.compare_exchange(
            stale_pending.bits(),
            GenerationTaggedState::new(7, WORKING).bits(),
            AtomicOrdering::Acquire,
            AtomicOrdering::Relaxed,
        )
        .is_err()
    );

    let observed = GenerationTaggedState::from_bits(slot.load(AtomicOrdering::Acquire));
    assert_eq!(observed.state_for(7), None);
    assert_eq!(observed.state_for(8), Some(PENDING));
    assert!(
        slot.compare_exchange(
            current_pending.bits(),
            GenerationTaggedState::new(8, WORKING).bits(),
            AtomicOrdering::Acquire,
            AtomicOrdering::Relaxed,
        )
        .is_ok()
    );
}

#[test]
fn one_shot_publication_has_one_winner_and_retains_its_value() {
    let published: Arc<PublishedOnce<usize>> = Arc::new(PublishedOnce::new());
    let mut publishers = std::vec::Vec::new();
    for value in 0..8usize {
        let published = Arc::clone(&published);
        publishers.push(thread::spawn(move || published.publish(value).is_ok()));
    }

    let mut winners = 0;
    for publisher in publishers {
        match publisher.join() {
            Ok(true) => winners += 1,
            Ok(false) => {}
            Err(_) => panic!("publication thread panicked"),
        }
    }
    assert_eq!(winners, 1);
    let Some(value) = published.get().copied() else {
        panic!("winning publication was not visible");
    };
    assert!(value < 8);
    match published.publish(9) {
        Ok(()) => panic!("second publication unexpectedly succeeded"),
        Err(error) => assert_eq!(error.into_value(), 9),
    }
    assert_eq!(published.get(), Some(&value));
}

#[test]
fn one_shot_publication_acquires_the_complete_payload() {
    #[derive(Debug, Eq, PartialEq)]
    struct Payload {
        first: usize,
        second: usize,
    }

    let published: Arc<PublishedOnce<Payload>> = Arc::new(PublishedOnce::new());
    let reader = Arc::clone(&published);
    let observer = thread::spawn(move || {
        loop {
            if let Some(value) = reader.get() {
                return (value.first, value.second);
            }
            thread::yield_now();
        }
    });

    assert!(
        published
            .publish(Payload {
                first: 0x1234,
                second: 0xabcd,
            })
            .is_ok()
    );
    match observer.join() {
        Ok(observed) => assert_eq!(observed, (0x1234, 0xabcd)),
        Err(_) => panic!("publication observer panicked"),
    }
}

#[test]
fn one_shot_publication_drops_the_retained_value_with_its_owner() {
    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let drops = Arc::new(AtomicUsize::new(0));
    {
        let published = PublishedOnce::new();
        assert!(published.publish(DropProbe(Arc::clone(&drops))).is_ok());
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}
