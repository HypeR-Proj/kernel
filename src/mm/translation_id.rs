// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Software address-space identities and leased architectural translation tags.
//!
//! Owners outlive individual hardware bindings. A lease pins its tag across
//! namespace rollover; callers flush locally for its epoch before installation
//! and retain it through hardware detachment or the last maintenance ack.

use hyper::mm::{
    TranslationEpochError, TranslationEpochPool, TranslationEpochSegment, TranslationLease,
    TranslationOwner,
};
use hyper::sync::InterruptSpinLock;

pub(crate) enum HostAsid {}
pub(crate) enum Stage2Vmid {}

type AsidPool = TranslationEpochPool<HostAsid>;
type VmidPool = TranslationEpochPool<Stage2Vmid>;
type AsidLock = InterruptSpinLock<AsidPool, crate::hal::irq::LocalMask>;
type VmidLock = InterruptSpinLock<VmidPool, crate::hal::irq::LocalMask>;

// SAFETY: These are the only pools for their private namespace types. They
// remain alive throughout the kernel lifetime, including every retained lease.
static ASIDS: AsidLock = InterruptSpinLock::new(unsafe { TranslationEpochPool::new() });
// SAFETY: Same unique-namespace lifetime contract as ASIDS.
static VMIDS: VmidLock = InterruptSpinLock::new(unsafe { TranslationEpochPool::new() });

pub(crate) trait IdentifierNamespace: Sized + 'static {
    fn with_pool<Result>(
        operation: impl FnOnce(&mut TranslationEpochPool<Self>) -> Result,
    ) -> Result;
}

impl IdentifierNamespace for HostAsid {
    fn with_pool<Result>(operation: impl FnOnce(&mut AsidPool) -> Result) -> Result {
        ASIDS.with(operation)
    }
}

impl IdentifierNamespace for Stage2Vmid {
    fn with_pool<Result>(operation: impl FnOnce(&mut VmidPool) -> Result) -> Result {
        VMIDS.with(operation)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Exhausted,
    Busy,
    InvalidToken,
    UnsupportedWidth,
    Allocation,
}

impl From<TranslationEpochError> for Error {
    fn from(error: TranslationEpochError) -> Self {
        match error {
            TranslationEpochError::Allocation => Self::Allocation,
            TranslationEpochError::Overflow => Self::Exhausted,
            TranslationEpochError::Busy => Self::Busy,
            TranslationEpochError::InvalidToken => Self::InvalidToken,
            TranslationEpochError::InvalidWidth | TranslationEpochError::WidthChanged => {
                Self::UnsupportedWidth
            }
        }
    }
}

#[must_use = "a software identity reservation must be activated or cancelled"]
pub(crate) struct IdentifierReservation<Namespace: IdentifierNamespace> {
    owner: Option<TranslationOwner<Namespace>>,
}

impl<Namespace: IdentifierNamespace> IdentifierReservation<Namespace> {
    pub(crate) fn generation(&self) -> u64 {
        self.owner.as_ref().map_or(0, TranslationOwner::serial)
    }

    pub(crate) fn activate(mut self) -> Result<ActiveIdentifier<Namespace>, Error> {
        Ok(ActiveIdentifier {
            owner: Some(self.owner.take().ok_or(Error::InvalidToken)?),
        })
    }
}

impl<Namespace: IdentifierNamespace> Drop for IdentifierReservation<Namespace> {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.take() {
            unregister(owner);
        }
    }
}

/// Published identity retained until the address space's retirement is acked.
/// Dropping it without retirement deliberately retains ownership: safe Rust
/// cannot make a still-published address space eligible for resource reuse.
#[must_use = "published identity must enter acknowledged retirement"]
pub(crate) struct ActiveIdentifier<Namespace: IdentifierNamespace> {
    owner: Option<TranslationOwner<Namespace>>,
}

impl<Namespace: IdentifierNamespace> ActiveIdentifier<Namespace> {
    pub(crate) fn generation(&self) -> u64 {
        self.owner.as_ref().map_or(0, TranslationOwner::serial)
    }

    pub(crate) fn acquire(&self) -> Result<IdentifierLease<Namespace>, Error> {
        let owner = self.owner.as_ref().ok_or(Error::InvalidToken)?;
        let token = Namespace::with_pool(|pool| pool.acquire(owner, None))?;
        Ok(IdentifierLease { token: Some(token) })
    }

    pub(crate) fn begin_retirement(mut self) -> Result<RetiringIdentifier<Namespace>, Error> {
        Ok(RetiringIdentifier {
            owner: Some(self.owner.take().ok_or(Error::InvalidToken)?),
        })
    }
}

/// Scoped hardware ownership. Release only after detaching the hardware or
/// receiving every acknowledgement for a maintenance operation using this tag.
#[must_use = "retain the lease throughout hardware use and maintenance"]
pub(crate) struct IdentifierLease<Namespace: IdentifierNamespace> {
    token: Option<TranslationLease<Namespace>>,
}

impl<Namespace: IdentifierNamespace> IdentifierLease<Namespace> {
    pub(crate) fn value(&self) -> u16 {
        self.token.as_ref().map_or(0, |token| token.binding().id())
    }

    pub(crate) fn generation(&self) -> u64 {
        self.token
            .as_ref()
            .map_or(0, |token| token.binding().owner())
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.token.as_ref().map_or(0, TranslationLease::epoch)
    }
}

impl<Namespace: IdentifierNamespace> Drop for IdentifierLease<Namespace> {
    fn drop(&mut self) {
        if let Some(token) = self.token.take()
            && Namespace::with_pool(|pool| pool.release(token)).is_err()
        {
            hyper::debug::invariant_failure("translation identifier lease release");
        }
    }
}

#[must_use = "retiring identity must be retained until invalidation is acknowledged"]
pub(crate) struct RetiringIdentifier<Namespace: IdentifierNamespace> {
    owner: Option<TranslationOwner<Namespace>>,
}

impl<Namespace: IdentifierNamespace> RetiringIdentifier<Namespace> {
    pub(crate) fn generation(&self) -> u64 {
        self.owner.as_ref().map_or(0, TranslationOwner::serial)
    }

    /// # Safety
    /// Every CPU which could retain translations or an in-flight walk must
    /// have acknowledged retirement. No execution or maintenance lease remains.
    pub(crate) unsafe fn complete(mut self) -> Result<(), Error> {
        let owner = self.owner.take().ok_or(Error::InvalidToken)?;
        unregister(owner);
        Ok(())
    }
}

fn unregister<Namespace: IdentifierNamespace>(owner: TranslationOwner<Namespace>) {
    if Namespace::with_pool(|pool| pool.unregister_owner(owner)).is_err() {
        hyper::debug::invariant_failure("translation owner retired with outstanding lease");
    }
    // Unlink under the pool lock, destroy outside it. Detaching a segment
    // advances the epoch so re-expansion cannot erase tag reuse history.
    while let Some(segment) = Namespace::with_pool(TranslationEpochPool::take_unused_segment) {
        drop(segment);
    }
}

pub(crate) fn reserve<Namespace: IdentifierNamespace>(
    width: u8,
) -> Result<IdentifierReservation<Namespace>, Error> {
    let mut spare = None;
    let owner = loop {
        if let Some(owner) = Namespace::with_pool(|pool| pool.register_owner(width, &mut spare))? {
            break owner;
        }
        spare = Some(TranslationEpochSegment::try_new()?);
    };
    Ok(IdentifierReservation { owner: Some(owner) })
}

/// Force real namespace rollover without installing synthetic owners into
/// hardware. An anchor proves that a pinned tag survives the rollover.
#[cfg(all(
    all(feature = "kernel-test", feature = "kernel-self-test"),
    any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)
))]
pub(crate) fn test_rollover<Namespace: IdentifierNamespace>(width: u8) -> Result<(), Error> {
    let anchor = reserve::<Namespace>(width)?.activate()?;
    let pinned = anchor.acquire()?;
    let before = Namespace::with_pool(|pool| pool.epoch());
    let result = (|| {
        for _ in 0..(1usize << width) {
            let owner = reserve::<Namespace>(width)?.activate()?;
            let lease = owner.acquire()?;
            drop(lease);
            // SAFETY: This synthetic owner was never installed in hardware;
            // there can be neither cached translations nor an outstanding walk.
            unsafe { owner.begin_retirement()?.complete()? };
            if Namespace::with_pool(|pool| pool.epoch()) != before {
                let retained = anchor.acquire()?;
                if retained.value() != pinned.value()
                    || retained.generation() != pinned.generation()
                    || retained.epoch() == before
                {
                    return Err(Error::InvalidToken);
                }
                return Ok(());
            }
        }
        Err(Error::Exhausted)
    })();
    drop(pinned);
    // SAFETY: The anchor also has never been installed into hardware.
    unsafe { anchor.begin_retirement()?.complete()? };
    result
}
