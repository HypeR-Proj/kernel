// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Hierarchical multi-resource accounting with owned charge transactions.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use hyper::mm::{AllocationError, FallibleArc};
use hyper::sync::InterruptSpinLock;

#[cfg(not(test))]
type DomainLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

#[cfg(test)]
struct TestInterruptMask;

#[cfg(test)]
impl hyper::hal::interrupt::InterruptMask for TestInterruptMask {
    type State = ();

    fn save_and_disable() -> Self::State {}

    fn restore(_: Self::State) {}

    fn wait_for_lock_owner() {
        std::thread::yield_now();
    }
}

#[cfg(test)]
type DomainLock<T> = InterruptSpinLock<T, TestInterruptMask>;

/// Maximum root-to-leaf domain count, including both endpoints.
///
/// A finite depth keeps reservation, commit, and Drop rollback on a bounded
/// kernel stack while preserving allocation-free transaction completion.
const MAX_DOMAIN_DEPTH: usize = 32;
const RESOURCE_KIND_COUNT: usize = 19;
/// Maximum distinct dimensions in one atomic accounting transaction.
///
/// Charges are intentionally compact because their linear owners are embedded
/// throughout kernel transactions. Limits and diagnostic snapshots retain the
/// complete dense vector below. The widest production request is the native
/// address-space charge, which uses all six entries.
const MAX_CHARGE_DIMENSIONS: usize = 6;
const RESOURCE_KIND_MASK: u32 = (1_u32 << RESOURCE_KIND_COUNT) - 1;
const OVERFLOWED_AMOUNT_MASK: u32 = 1_u32 << 31;

static NEXT_DOMAIN_ID: AtomicU64 = AtomicU64::new(1);

/// Stable diagnostic identity which does not grant authority over a domain.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ResourceDomainId(u64);

impl ResourceDomainId {
    fn allocate() -> Result<Self, ResourceError> {
        let mut current = NEXT_DOMAIN_ID.load(Ordering::Relaxed);
        loop {
            if current == 0 {
                return Err(ResourceError::DomainIdExhausted);
            }
            let Some(next) = current.checked_add(1) else {
                return Err(ResourceError::DomainIdExhausted);
            };
            match NEXT_DOMAIN_ID.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(Self(current)),
                Err(observed) => current = observed,
            }
        }
    }
}

/// Independently limited resource dimensions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub(crate) enum ResourceKind {
    KernelMemoryBytes,
    Processes,
    Threads,
    Handles,
    KernelObjects,
    CommittedPages,
    PinnedPages,
    GuestPages,
    IpcMessages,
    IpcBytes,
    IpcHandles,
    Subscriptions,
    Timers,
    VirtualMachines,
    VirtualCpus,
    DeviceLeases,
    DmaMappings,
    UserAddressSpaces,
    UserMappings,
}

impl ResourceKind {
    const ALL: [Self; RESOURCE_KIND_COUNT] = [
        Self::KernelMemoryBytes,
        Self::Processes,
        Self::Threads,
        Self::Handles,
        Self::KernelObjects,
        Self::CommittedPages,
        Self::PinnedPages,
        Self::GuestPages,
        Self::IpcMessages,
        Self::IpcBytes,
        Self::IpcHandles,
        Self::Subscriptions,
        Self::Timers,
        Self::VirtualMachines,
        Self::VirtualCpus,
        Self::DeviceLeases,
        Self::DmaMappings,
        Self::UserAddressSpaces,
        Self::UserMappings,
    ];

    const fn index(self) -> usize {
        self as usize
    }
}

/// One compact atomic resource delta.
///
/// Set bits identify dimensions and `values` stores their nonzero values in
/// ascending [`ResourceKind`] order. An overflow marker is sticky so fluent
/// constant construction cannot silently discard a dimension; admission then
/// rejects the malformed request before changing any counter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ResourceAmount {
    mask: u32,
    values: [u64; MAX_CHARGE_DIMENSIONS],
}

impl ResourceAmount {
    pub(crate) const ZERO: Self = Self {
        mask: 0,
        values: [0; MAX_CHARGE_DIMENSIONS],
    };

    /// Returns a new delta with the selected dimension replaced by `value`.
    ///
    /// Supplying zero removes the dimension. Adding more than
    /// [`MAX_CHARGE_DIMENSIONS`] distinct nonzero dimensions produces a delta
    /// which [`ResourceDomain::reserve`] rejects atomically.
    pub(crate) const fn with(mut self, kind: ResourceKind, value: u64) -> Self {
        if self.overflowed() {
            return self;
        }

        let bit = 1_u32 << kind.index();
        let index = (self.mask & bit.wrapping_sub(1)).count_ones() as usize;
        if self.mask & bit != 0 {
            if value != 0 {
                self.values[index] = value;
                return self;
            }
            let mut cursor = index;
            let length = self.mask.count_ones() as usize;
            while cursor + 1 < length {
                self.values[cursor] = self.values[cursor + 1];
                cursor += 1;
            }
            self.values[length - 1] = 0;
            self.mask &= !bit;
            return self;
        }

        if value == 0 {
            return self;
        }
        let length = self.mask.count_ones() as usize;
        if length == MAX_CHARGE_DIMENSIONS {
            self.mask |= OVERFLOWED_AMOUNT_MASK;
            return self;
        }
        let mut cursor = length;
        while cursor > index {
            self.values[cursor] = self.values[cursor - 1];
            cursor -= 1;
        }
        self.values[index] = value;
        self.mask |= bit;
        self
    }

    pub(crate) const fn get(self, kind: ResourceKind) -> u64 {
        let bit = 1_u32 << kind.index();
        if self.mask & bit == 0 {
            return 0;
        }
        self.values[(self.mask & bit.wrapping_sub(1)).count_ones() as usize]
    }

    pub(crate) const fn is_empty(self) -> bool {
        self.mask == 0
    }

    const fn overflowed(self) -> bool {
        self.mask & OVERFLOWED_AMOUNT_MASK != 0
    }

    fn entries(&self) -> ResourceAmountEntries<'_> {
        ResourceAmountEntries {
            amount: self,
            remaining: self.mask & RESOURCE_KIND_MASK,
        }
    }
}

struct ResourceAmountEntries<'amount> {
    amount: &'amount ResourceAmount,
    remaining: u32,
}

impl Iterator for ResourceAmountEntries<'_> {
    type Item = (ResourceKind, u64);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let raw_index = self.remaining.trailing_zeros() as usize;
        let bit = 1_u32 << raw_index;
        self.remaining &= !bit;
        let kind = ResourceKind::ALL[raw_index];
        Some((kind, self.amount.get(kind)))
    }
}

/// Dense storage used where all resource dimensions must remain observable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResourceVector([u64; RESOURCE_KIND_COUNT]);

impl ResourceVector {
    #[cfg_attr(
        not(test),
        cfg(all(feature = "kernel-test", feature = "kernel-self-test"))
    )]
    const ZERO: Self = Self([0; RESOURCE_KIND_COUNT]);

    const fn get(self, kind: ResourceKind) -> u64 {
        self.0[kind.index()]
    }
}

/// Local ceilings. Ancestors remain independently authoritative ceilings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResourceLimits(ResourceVector);

impl ResourceLimits {
    pub(crate) const UNLIMITED: Self = Self(ResourceVector([u64::MAX; RESOURCE_KIND_COUNT]));

    pub(crate) const fn with(mut self, kind: ResourceKind, limit: u64) -> Self {
        self.0.0[kind.index()] = limit;
        self
    }

    pub(crate) const fn get(self, kind: ResourceKind) -> u64 {
        self.0.get(kind)
    }
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self::UNLIMITED
    }
}

/// One race-safe domain-local accounting snapshot.
///
/// `total` is authoritative. Concurrent commit or release may make the
/// pending/committed split conservatively stale, so that split is diagnostic;
/// admission and local-limit changes use `total` under the control lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(
    not(test),
    cfg(all(feature = "kernel-test", feature = "kernel-self-test"))
)]
pub(crate) struct ResourceUsage {
    total: ResourceVector,
    pending: ResourceVector,
}

#[cfg_attr(
    not(test),
    cfg(all(feature = "kernel-test", feature = "kernel-self-test"))
)]
impl ResourceUsage {
    pub(crate) fn committed(self, kind: ResourceKind) -> u64 {
        match self.total.get(kind).checked_sub(self.pending.get(kind)) {
            Some(value) => value,
            None => accounting_invariant_violation(),
        }
    }

    #[cfg(test)]
    pub(crate) const fn pending(self, kind: ResourceKind) -> u64 {
        self.pending.get(kind)
    }

    pub(crate) fn total(self, kind: ResourceKind) -> u64 {
        self.total.get(kind)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResourceError {
    Allocation,
    DomainIdExhausted,
    HierarchyTooDeep,
    EmptyCharge,
    TooManyChargeDimensions,
    DomainInactive(ResourceDomainId),
    LimitExceeded {
        domain: ResourceDomainId,
        resource: ResourceKind,
        limit: u64,
        used: u64,
        requested: u64,
    },
    UsageOverflow {
        domain: ResourceDomainId,
        resource: ResourceKind,
    },
    #[cfg_attr(
        not(test),
        cfg_attr(
            not(all(feature = "kernel-test", feature = "kernel-self-test")),
            expect(
                dead_code,
                reason = "Limit updates currently have only contract-test consumers"
            )
        )
    )]
    LimitBelowUsage {
        domain: ResourceDomainId,
        resource: ResourceKind,
        limit: u64,
        used: u64,
    },
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Resource-domain administrative retirement is host-tested but not wired into production policy"
        )
    )]
    OutstandingUsage,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Resource-domain administrative retirement is host-tested but not wired into production policy"
        )
    )]
    ActiveChildren,
    ChildCountExhausted,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Resource-domain administrative retirement is host-tested but not wired into production policy"
        )
    )]
    RetirementNotStarted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(test)]
pub(crate) struct RetirementSnapshot {
    pub(crate) usage: ResourceUsage,
    pub(crate) active_children: usize,
}

#[cfg(test)]
impl RetirementSnapshot {
    pub(crate) fn is_quiescent(self) -> bool {
        self.active_children == 0
            && ResourceKind::ALL
                .iter()
                .all(|kind| self.usage.total(*kind) == 0)
    }
}

impl From<AllocationError> for ResourceError {
    fn from(_: AllocationError) -> Self {
        Self::Allocation
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DomainLifecycle {
    Active,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Resource-domain administrative retirement is host-tested but not wired into production policy"
        )
    )]
    Retiring,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Resource-domain administrative retirement is host-tested but not wired into production policy"
        )
    )]
    Retired,
}

struct DomainControl {
    limits: ResourceLimits,
    lifecycle: DomainLifecycle,
}

struct DomainInner {
    id: ResourceDomainId,
    parent: Option<ResourceDomain>,
    // Owns the metadata quota until this domain is dropped.
    _metadata_charge: Option<CommittedCharge>,
    depth: usize,
    control: DomainLock<DomainControl>,
    active_children: AtomicUsize,
    object_published: AtomicBool,
    total: [AtomicU64; RESOURCE_KIND_COUNT],
    pending: [AtomicU64; RESOURCE_KIND_COUNT],
    #[cfg(test)]
    fail_child_allocation: AtomicBool,
}

impl Drop for DomainInner {
    fn drop(&mut self) {
        if self.active_children.load(Ordering::Relaxed) != 0 {
            accounting_invariant_violation();
        }
        for kind in ResourceKind::ALL {
            if self.total[kind.index()].load(Ordering::Relaxed) != 0
                || self.pending[kind.index()].load(Ordering::Relaxed) != 0
            {
                accounting_invariant_violation();
            }
        }

        let Some(parent) = self.parent.as_ref() else {
            return;
        };
        let previous = parent.inner.active_children.fetch_sub(1, Ordering::Release);
        if previous == 0 {
            accounting_invariant_violation();
        }
    }
}

const DOMAIN_METADATA_BYTES: u64 = FallibleArc::<DomainInner>::allocation_size() as u64;

fn domain_metadata_charge() -> ResourceAmount {
    ResourceAmount::ZERO
        .with(ResourceKind::KernelObjects, 1)
        .with(ResourceKind::KernelMemoryBytes, DOMAIN_METADATA_BYTES)
}

/// Shared authority over one node in a hierarchical accounting tree.
///
/// Children and charge owners retain their complete ancestor lifetime through
/// strong parent links. Admission visits root-to-leaf using one short lock at
/// a time; conservative atomic usage prevents siblings from overbooking their
/// shared ancestors. Failure and Drop release leaf-to-root. No public API
/// exposes a lock or invokes caller code while locked.
pub(crate) struct ResourceDomain {
    inner: FallibleArc<DomainInner>,
}

impl ResourceDomain {
    pub(crate) fn try_new_root(limits: ResourceLimits) -> Result<Self, ResourceError> {
        let inner = DomainInner {
            id: ResourceDomainId::allocate()?,
            parent: None,
            _metadata_charge: None,
            depth: 0,
            control: DomainLock::new(DomainControl {
                limits,
                lifecycle: DomainLifecycle::Active,
            }),
            active_children: AtomicUsize::new(0),
            object_published: AtomicBool::new(false),
            total: [const { AtomicU64::new(0) }; RESOURCE_KIND_COUNT],
            pending: [const { AtomicU64::new(0) }; RESOURCE_KIND_COUNT],
            #[cfg(test)]
            fail_child_allocation: AtomicBool::new(false),
        };
        Ok(Self {
            inner: FallibleArc::try_new(inner)?,
        })
    }

    /// Constructs and registers one child beneath this domain.
    ///
    /// Parent-sponsored metadata quota is committed before child publication.
    /// If registration or allocation fails, linear owners restore both the
    /// child count and the metadata charge without a special cleanup branch.
    pub(crate) fn try_new_child(&self, limits: ResourceLimits) -> Result<Self, ResourceError> {
        let Some(depth) = self.inner.depth.checked_add(1) else {
            return Err(ResourceError::HierarchyTooDeep);
        };
        if depth >= MAX_DOMAIN_DEPTH {
            return Err(ResourceError::HierarchyTooDeep);
        }
        let id = ResourceDomainId::allocate()?;
        // This reservation is the child-creation admission point. Retirement
        // may close the domain immediately afterward, but the pending/committed
        // metadata total keeps the domain non-quiescent until this pre-cutoff
        // creation either publishes its child or rolls back.
        let metadata_charge = self.reserve(domain_metadata_charge())?.commit();
        self.inner.control.with(|_| {
            self.inner
                .active_children
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |children| {
                    children.checked_add(1)
                })
                .map(|_| ())
                .map_err(|_| ResourceError::ChildCountExhausted)
        })?;

        let inner = DomainInner {
            id,
            parent: Some(self.clone()),
            _metadata_charge: Some(metadata_charge),
            depth,
            control: DomainLock::new(DomainControl {
                limits,
                lifecycle: DomainLifecycle::Active,
            }),
            active_children: AtomicUsize::new(0),
            object_published: AtomicBool::new(false),
            total: [const { AtomicU64::new(0) }; RESOURCE_KIND_COUNT],
            pending: [const { AtomicU64::new(0) }; RESOURCE_KIND_COUNT],
            #[cfg(test)]
            fail_child_allocation: AtomicBool::new(false),
        };
        #[cfg(test)]
        if self
            .inner
            .fail_child_allocation
            .swap(false, Ordering::Relaxed)
        {
            drop(inner);
            return Err(ResourceError::Allocation);
        }
        Ok(Self {
            inner: FallibleArc::try_new(inner)?,
        })
    }

    pub(crate) fn id(&self) -> ResourceDomainId {
        self.inner.id
    }

    /// Tests immutable quota ancestry without locking or allocating. Domain
    /// construction bounds this walk by `MAX_DOMAIN_DEPTH`.
    pub(crate) fn charges_domain(&self, id: ResourceDomainId) -> bool {
        let mut current = Some(self);
        while let Some(domain) = current {
            if domain.id() == id {
                return true;
            }
            current = domain.inner.parent.as_ref();
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn parent_id(&self) -> Option<ResourceDomainId> {
        self.inner.parent.as_ref().map(Self::id)
    }

    /// Reserves quota at this domain and every ancestor in one transaction.
    ///
    /// Pending usage counts against every limit immediately. The returned
    /// linear token must be committed or aborted; Drop performs exact local
    /// rollback without allocation or callbacks.
    pub(crate) fn reserve(
        &self,
        amount: ResourceAmount,
    ) -> Result<ChargeReservation, ResourceError> {
        if amount.overflowed() {
            return Err(ResourceError::TooManyChargeDimensions);
        }
        if amount.is_empty() {
            return Err(ResourceError::EmptyCharge);
        }
        let path = DomainPath::new(self);
        path.reserve(&amount)?;
        Ok(ChargeReservation {
            domain: Some(self.clone()),
            amount,
        })
    }

    #[cfg_attr(
        not(test),
        cfg(all(feature = "kernel-test", feature = "kernel-self-test"))
    )]
    pub(crate) fn usage(&self) -> ResourceUsage {
        self.inner.control.with(|_| ResourceUsage {
            total: load_total_counters(&self.inner.total),
            pending: load_counters(&self.inner.pending),
        })
    }

    #[cfg(test)]
    pub(crate) fn local_limits(&self) -> ResourceLimits {
        self.inner.control.with(|control| control.limits)
    }

    pub(super) fn claim_object_publication(&self) -> bool {
        self.inner
            .object_published
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn abort_object_publication(&self) {
        if self
            .inner
            .object_published
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            accounting_invariant_violation();
        }
    }

    /// Replaces local ceilings without changing usage or ancestor policy.
    ///
    /// Child ceilings may exceed an ancestor ceiling: the effective limit is
    /// the minimum remaining capacity along the path. This permits a parent
    /// policy to tighten or relax without rewriting every descendant.
    #[cfg_attr(
        not(test),
        cfg(all(feature = "kernel-test", feature = "kernel-self-test"))
    )]
    pub(crate) fn set_local_limits(&self, limits: ResourceLimits) -> Result<(), ResourceError> {
        self.inner.control.with(|control| {
            if control.lifecycle != DomainLifecycle::Active {
                return Err(ResourceError::DomainInactive(self.id()));
            }
            for kind in ResourceKind::ALL {
                let used = self.inner.total[kind.index()].load(Ordering::Acquire);
                let limit = limits.get(kind);
                if limit < used {
                    return Err(ResourceError::LimitBelowUsage {
                        domain: self.id(),
                        resource: kind,
                        limit,
                        used,
                    });
                }
            }
            control.limits = limits;
            Ok(())
        })
    }

    /// Permanently closes admission at this node before existing work drains.
    ///
    /// Descendant reservations visit this node and therefore observe the same
    /// cutoff. Reservations admitted before the cutoff remain committable.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Resource-domain administrative retirement is host-tested but not wired into production policy"
        )
    )]
    pub(crate) fn begin_retirement(&self) -> Result<(), ResourceError> {
        self.inner.control.with(|control| match control.lifecycle {
            DomainLifecycle::Active => {
                control.lifecycle = DomainLifecycle::Retiring;
                Ok(())
            }
            DomainLifecycle::Retiring => Ok(()),
            DomainLifecycle::Retired => Err(ResourceError::DomainInactive(self.id())),
        })
    }

    #[cfg(test)]
    pub(crate) fn retirement_snapshot(&self) -> Result<RetirementSnapshot, ResourceError> {
        self.inner.control.with(|control| {
            if control.lifecycle == DomainLifecycle::Active {
                return Err(ResourceError::RetirementNotStarted);
            }
            Ok(RetirementSnapshot {
                usage: ResourceUsage {
                    total: load_total_counters(&self.inner.total),
                    pending: load_counters(&self.inner.pending),
                },
                active_children: self.inner.active_children.load(Ordering::Acquire),
            })
        })
    }

    /// Publishes terminal retirement only after sponsored ownership drains.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Resource-domain administrative retirement is host-tested but not wired into production policy"
        )
    )]
    pub(crate) fn finish_retirement(&self) -> Result<(), ResourceError> {
        self.inner.control.with(|control| {
            match control.lifecycle {
                DomainLifecycle::Active => return Err(ResourceError::RetirementNotStarted),
                DomainLifecycle::Retired => {
                    return Err(ResourceError::DomainInactive(self.id()));
                }
                DomainLifecycle::Retiring => {}
            }
            if self.inner.active_children.load(Ordering::Acquire) != 0 {
                return Err(ResourceError::ActiveChildren);
            }
            if ResourceKind::ALL
                .iter()
                .any(|kind| self.inner.total[kind.index()].load(Ordering::Acquire) != 0)
            {
                return Err(ResourceError::OutstandingUsage);
            }
            control.lifecycle = DomainLifecycle::Retired;
            Ok(())
        })
    }

    #[cfg(test)]
    pub(crate) fn fail_next_child_allocation_for_test(&self) {
        self.inner
            .fail_child_allocation
            .store(true, Ordering::Relaxed);
    }
}

impl Clone for ResourceDomain {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct DomainPath<'domain> {
    nodes: [Option<&'domain DomainInner>; MAX_DOMAIN_DEPTH],
    len: usize,
}

impl<'domain> DomainPath<'domain> {
    fn new(target: &'domain ResourceDomain) -> Self {
        let len = target.inner.depth + 1;
        let mut nodes = [None; MAX_DOMAIN_DEPTH];
        let mut cursor = Some(target);
        let mut index = len;
        while let Some(domain) = cursor {
            if index == 0 {
                accounting_invariant_violation();
            }
            index -= 1;
            nodes[index] = Some(&*domain.inner);
            cursor = domain.inner.parent.as_ref();
        }
        if index != 0 {
            accounting_invariant_violation();
        }
        Self { nodes, len }
    }

    fn node(&self, index: usize) -> &DomainInner {
        match self.nodes.get(index).copied().flatten() {
            Some(node) => node,
            None => accounting_invariant_violation(),
        }
    }

    /// Admits one node at a time. Prefix usage is conservative and grants no
    /// authority; a later failure removes that prefix leaf-to-root.
    fn reserve(&self, amount: &ResourceAmount) -> Result<(), ResourceError> {
        let mut admitted = 0;
        while admitted < self.len {
            let domain = self.node(admitted);
            let result = domain.control.with(|control| {
                if control.lifecycle != DomainLifecycle::Active {
                    return Err(ResourceError::DomainInactive(domain.id));
                }
                for (kind, requested) in amount.entries() {
                    let used = domain.total[kind.index()].load(Ordering::Relaxed);
                    let Some(projected) = used.checked_add(requested) else {
                        return Err(ResourceError::UsageOverflow {
                            domain: domain.id,
                            resource: kind,
                        });
                    };
                    let limit = control.limits.get(kind);
                    if projected > limit {
                        return Err(ResourceError::LimitExceeded {
                            domain: domain.id,
                            resource: kind,
                            limit,
                            used,
                            requested,
                        });
                    }
                }
                add_counters(&domain.total, amount);
                add_counters(&domain.pending, amount);
                Ok(())
            });
            if let Err(error) = result {
                self.release_pending_prefix(admitted, amount);
                return Err(error);
            }
            admitted += 1;
        }
        Ok(())
    }

    fn commit(&self, amount: &ResourceAmount) {
        for index in (0..self.len).rev() {
            subtract_pending_counters(&self.node(index).pending, amount);
        }
    }

    fn release_pending(&self, amount: &ResourceAmount) {
        self.release_pending_prefix(self.len, amount);
    }

    fn release_pending_prefix(&self, admitted: usize, amount: &ResourceAmount) {
        for index in (0..admitted).rev() {
            let domain = self.node(index);
            subtract_pending_counters(&domain.pending, amount);
            subtract_total_counters(&domain.total, amount);
        }
    }

    fn release_committed(&self, amount: &ResourceAmount) {
        for index in (0..self.len).rev() {
            subtract_total_counters(&self.node(index).total, amount);
        }
    }
}

#[cfg_attr(
    not(test),
    cfg(all(feature = "kernel-test", feature = "kernel-self-test"))
)]
fn load_counters(counters: &[AtomicU64; RESOURCE_KIND_COUNT]) -> ResourceVector {
    let mut amount = ResourceVector::ZERO;
    for kind in ResourceKind::ALL {
        amount.0[kind.index()] = counters[kind.index()].load(Ordering::Relaxed);
    }
    amount
}

#[cfg_attr(
    not(test),
    cfg(all(feature = "kernel-test", feature = "kernel-self-test"))
)]
fn load_total_counters(counters: &[AtomicU64; RESOURCE_KIND_COUNT]) -> ResourceVector {
    let mut amount = ResourceVector::ZERO;
    for kind in ResourceKind::ALL {
        // Acquire pairs with the releasing total decrement performed after a
        // pending decrement. Observing the new total therefore also observes
        // a pending value no greater than that total.
        amount.0[kind.index()] = counters[kind.index()].load(Ordering::Acquire);
    }
    amount
}

fn add_counters(counters: &[AtomicU64; RESOURCE_KIND_COUNT], amount: &ResourceAmount) {
    for (kind, value) in amount.entries() {
        counters[kind.index()].fetch_add(value, Ordering::Relaxed);
    }
}

fn subtract_pending_counters(counters: &[AtomicU64; RESOURCE_KIND_COUNT], amount: &ResourceAmount) {
    for (kind, value) in amount.entries() {
        let previous = counters[kind.index()].fetch_sub(value, Ordering::Relaxed);
        if previous < value {
            accounting_invariant_violation();
        }
    }
}

fn subtract_total_counters(counters: &[AtomicU64; RESOURCE_KIND_COUNT], amount: &ResourceAmount) {
    for (kind, value) in amount.entries() {
        // Every pending decrement is sequenced before this publication. AcqRel
        // also carries pending publications from an earlier concurrent total
        // RMW, so a reader of the newest total cannot miss either decrement on
        // a weakly ordered machine.
        let previous = counters[kind.index()].fetch_sub(value, Ordering::AcqRel);
        if previous < value {
            accounting_invariant_violation();
        }
    }
}

/// Unpublished quota ownership counted as pending usage.
#[must_use = "dropping a reservation rolls its pending charge back"]
pub(crate) struct ChargeReservation {
    domain: Option<ResourceDomain>,
    amount: ResourceAmount,
}

impl ChargeReservation {
    #[cfg(test)]
    pub(crate) fn domain_id(&self) -> ResourceDomainId {
        match self.domain.as_ref() {
            Some(domain) => domain.id(),
            None => accounting_invariant_violation(),
        }
    }

    #[cfg(test)]
    pub(crate) const fn amount(&self) -> ResourceAmount {
        self.amount
    }

    /// Atomically changes pending ownership into committed ownership.
    pub(crate) fn commit(mut self) -> CommittedCharge {
        let domain = match self.domain.as_ref() {
            Some(domain) => domain,
            None => accounting_invariant_violation(),
        };
        DomainPath::new(domain).commit(&self.amount);
        let domain = match self.domain.take() {
            Some(domain) => domain,
            None => accounting_invariant_violation(),
        };
        CommittedCharge {
            domain,
            amount: self.amount,
        }
    }

    /// Explicitly restores pending quota before returning.
    #[cfg(test)]
    pub(crate) fn abort(mut self) {
        let domain = match self.domain.as_ref() {
            Some(domain) => domain,
            None => accounting_invariant_violation(),
        };
        DomainPath::new(domain).release_pending(&self.amount);
        self.domain = None;
    }
}

impl Drop for ChargeReservation {
    fn drop(&mut self) {
        let Some(domain) = self.domain.as_ref() else {
            return;
        };
        DomainPath::new(domain).release_pending(&self.amount);
    }
}

/// Published resource ownership counted as committed usage.
///
/// The charge is intentionally linear. Moving it transfers accounting
/// ownership; Drop releases the charge from the leaf and every ancestor.
#[must_use = "the committed charge owns quota until it is dropped"]
pub(crate) struct CommittedCharge {
    domain: ResourceDomain,
    amount: ResourceAmount,
}

impl CommittedCharge {
    pub(crate) fn charges_domain(&self, id: ResourceDomainId) -> bool {
        self.domain.charges_domain(id)
    }

    /// Separates an already admitted subset without changing usage counters.
    /// The returned owner must outlive the storage whose charge it carries.
    pub(crate) fn split_off(&mut self, amount: ResourceAmount) -> Self {
        if amount.overflowed() {
            accounting_invariant_violation();
        }
        let mut remaining = self.amount;
        for (kind, value) in amount.entries() {
            let Some(left) = remaining.get(kind).checked_sub(value) else {
                accounting_invariant_violation();
            };
            remaining = remaining.with(kind, left);
        }
        self.amount = remaining;
        Self {
            domain: self.domain.clone(),
            amount,
        }
    }

    #[cfg(test)]
    pub(crate) fn domain_id(&self) -> ResourceDomainId {
        self.domain.id()
    }

    #[cfg(test)]
    pub(crate) const fn amount(&self) -> ResourceAmount {
        self.amount
    }

    /// Extends this linear owner with another atomic resource delta.
    ///
    /// Admission happens before the owner changes. On success the temporary
    /// committed charge is disarmed and this owner releases the combined usage
    /// exactly once. This permits resources which grow incrementally, such as
    /// sparse machine address spaces, to retain one bounded accounting owner
    /// rather than allocating one owner per page.
    #[cfg(test)]
    pub(crate) fn try_extend(&mut self, additional: ResourceAmount) -> Result<(), ResourceError> {
        if additional.overflowed() {
            return Err(ResourceError::TooManyChargeDimensions);
        }
        if additional.is_empty() {
            return Ok(());
        }

        let mut combined = self.amount;
        for (kind, value) in additional.entries() {
            let Some(value) = combined.get(kind).checked_add(value) else {
                return Err(ResourceError::UsageOverflow {
                    domain: self.domain.id(),
                    resource: kind,
                });
            };
            combined = combined.with(kind, value);
        }
        if combined.overflowed() {
            return Err(ResourceError::TooManyChargeDimensions);
        }

        let extension = self.domain.reserve(additional)?.commit();
        self.absorb_pre_admitted(extension);
        Ok(())
    }

    /// Coalesces an already committed delta from the same domain.
    ///
    /// This is the irreversible tail for resource creation which had to reserve
    /// quota before publishing an allocation. Resource-domain admission proves
    /// the combined values cannot overflow; a mismatch here is kernel ownership
    /// corruption rather than a recoverable resource failure.
    pub(crate) fn absorb_pre_admitted(&mut self, mut extension: Self) {
        if self.domain.id() != extension.domain.id() {
            accounting_invariant_violation();
        }
        let mut combined = self.amount;
        for (kind, value) in extension.amount.entries() {
            let Some(value) = combined.get(kind).checked_add(value) else {
                accounting_invariant_violation();
            };
            combined = combined.with(kind, value);
        }
        if combined.overflowed() {
            accounting_invariant_violation();
        }
        self.amount = combined;
        // The combined owner now carries the exact counter delta. Clearing the
        // temporary amount makes its Drop release no counters while still
        // releasing its strong ResourceDomain reference normally.
        extension.amount = ResourceAmount::ZERO;
    }
}

impl Drop for CommittedCharge {
    fn drop(&mut self) {
        DomainPath::new(&self.domain).release_committed(&self.amount);
    }
}

#[cold]
fn accounting_invariant_violation() -> ! {
    hyper::debug::invariant_failure("resource domain accounting invariant")
}
