// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! CPU-owned intrusive ready queues with allocation-free algorithm indices.
//!
//! Linked lists retain residence and control-queue invariants. AVL indices
//! supply each algorithm/role candidate to the separate arbitration policy.

use alloc::boxed::Box;

use hyper::cpu::CpuIndex;

use super::Error;
use super::algorithms::PolicyRuntime;
use super::algorithms::arbitration::{Candidate, CpuWindow, Demand, DispatchKey};
use super::algorithms::ready_index::{Links, MutationScratch, Nodes, Tree};
use super::registry::{CpuThreadTableAuthority, ThreadControlAuthority, ThreadTableWriteAuthority};
use crate::task::wait::ThreadQueue;
use {
    crate::task::policy::FairBackend, crate::task::policy::PRIORITY_LEVELS,
    crate::task::policy::SchedulingParameters, crate::task::policy::SchedulingPolicy,
    crate::task::policy::SchedulingRole,
};
use {
    crate::task::thread::QueueLinks, crate::task::thread::QueueMembership,
    crate::task::thread::ReadyTreeLinks, crate::task::thread::ThreadId,
};

pub(super) type QueueScratch = Box<MutationScratch<ThreadId>>;

const ROLE_COUNT: usize = 3;
const FAIR: usize = ROLE_COUNT;
const BUDGET_READY: usize = ROLE_COUNT * 2;
const BUDGET_EXHAUSTED: usize = ROLE_COUNT * 3;
const INDEX_COUNT: usize = ROLE_COUNT * 4;

/// Runnable classes owned by one CPU.
///
/// Idle is intentionally absent: its Thread is selected only when every
/// runnable class is empty.
pub(super) struct CpuRunQueue {
    realtime: FixedPriorityFifo,
    fair: FairQueue,
    indices: [Tree<ThreadId>; INDEX_COUNT],
    // All indices share one path: their mutations are synchronous and the
    // CPU queue lock excludes both remote access and local IRQ reentry.
    scratch: QueueScratch,
    budget_epoch: Option<u64>,
    budget_threads: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReadyThread {
    pub id: ThreadId,
    pub policy: SchedulingPolicy,
    pub key: DispatchKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Selection {
    pub ready: Option<ReadyThread>,
    pub demand: Demand,
    pub fair_min_pass: Option<u128>,
}

/// Combined transition-lock and CPU-lock authority for one ready queue.
///
/// Coordinator-owned candidates may enter the CPU domain, while existing
/// ready/current entities are accessed only through the matching CPU token.
pub(super) struct ReadyQueueAuthority<'table, 'cpu> {
    coordinator: ThreadTableWriteAuthority<'table>,
    cpu: CpuThreadTableAuthority<'cpu>,
}

/// Pure CPU-local authority for already CPU-owned current/ready entities.
///
/// It cannot publish coordinator-owned Threads and cannot observe global
/// control links. This is the only authority admitted by local scheduling.
pub(super) struct LocalReadyQueueAuthority<'cpu> {
    cpu: CpuThreadTableAuthority<'cpu>,
}

impl<'cpu> LocalReadyQueueAuthority<'cpu> {
    pub(super) const fn new(cpu: CpuThreadTableAuthority<'cpu>) -> Self {
        Self { cpu }
    }
}

/// TransitionLock-owned authority for global waiting/terminated queues.
///
/// It deliberately has no CPU scheduling token: control links are a separate
/// Thread cell, so adjacent nodes may have schedules owned by different CPUs.
pub(super) struct ControlQueueAuthority<'table> {
    control: ThreadControlAuthority<'table>,
}

impl<'table> ControlQueueAuthority<'table> {
    pub(super) const fn new(control: ThreadControlAuthority<'table>) -> Self {
        Self { control }
    }
}

impl<'table, 'cpu> ReadyQueueAuthority<'table, 'cpu> {
    pub(super) const fn new(
        coordinator: ThreadTableWriteAuthority<'table>,
        cpu: CpuThreadTableAuthority<'cpu>,
    ) -> Self {
        Self { coordinator, cpu }
    }

    /// Mutates schedule state through the authority selected by its owner locator.
    fn with_schedule_mut<R>(
        &mut self,
        id: ThreadId,
        operation: impl for<'schedule> FnOnce(
            &'schedule mut crate::task::thread::ThreadScheduleState,
        ) -> R,
    ) -> Result<R, Error> {
        let owner = self
            .coordinator
            .with_thread(id, |thread| thread.schedule_owner_cpu())?;
        match owner {
            None => self
                .coordinator
                .with_thread_mut(id, |thread| thread.with_coordinator_schedule_mut(operation)),
            Some(cpu) if cpu == self.cpu.cpu() => self
                .cpu
                .with_thread_mut(id, |_thread, schedule| operation(schedule)),
            Some(_) => Err(Error::InvalidThreadState),
        }
    }
}

impl CpuRunQueue {
    pub fn prepare_scratch() -> Result<QueueScratch, Error> {
        hyper::mm::try_box(MutationScratch::new(ThreadId::BOOTSTRAP)).map_err(|_| Error::Allocation)
    }

    pub fn new(scratch: QueueScratch) -> Self {
        Self {
            scratch,
            realtime: FixedPriorityFifo::new(),
            fair: FairQueue::new(),
            indices: [Tree::new(); INDEX_COUNT],
            budget_epoch: None,
            budget_threads: 0,
        }
    }
    pub const fn len(&self) -> usize {
        self.realtime.len() + self.fair.len()
    }
    pub const fn budget_len(&self) -> usize {
        self.budget_threads
    }
    pub const fn real_time_len(&self) -> usize {
        self.realtime.len()
    }
    pub const fn fair_len(&self) -> usize {
        self.fair.len()
    }

    /// Selects one minimum from each algorithm/role group, then arbitrates.
    /// There is no walk over all runnable Threads on the dispatch hot path.
    /// Expired Budget nodes are reactivated lazily once per common period.
    pub fn peek_next(
        &mut self,
        threads: &mut impl ReadyQueueAccess,
        cpu: CpuIndex,
        now: u64,
        window: &mut CpuWindow,
        current: Option<Candidate>,
    ) -> Result<Selection, Error> {
        let epoch = now / window.period_ns();
        if self.budget_epoch != Some(epoch) {
            for role in 0..ROLE_COUNT {
                let exhausted = role + BUDGET_EXHAUSTED;
                while let Some(id) = self.indices[exhausted].first(&mut IndexNodes(threads))? {
                    self.indices[exhausted]
                        .remove(&mut IndexNodes(threads), id, &mut self.scratch)
                        .unwrap_or_else(|_| queue_invariant());
                    threads.refresh_runtime(id, now)?;
                    self.indices[role + BUDGET_READY]
                        .insert(&mut IndexNodes(threads), id, &mut self.scratch)
                        .unwrap_or_else(|_| queue_invariant());
                }
            }
            self.budget_epoch = Some(epoch);
        }
        // Keep only identities across the demand/selection passes. A complete
        // Candidate contains policy and accounting values; retaining twelve
        // copies (and an array iterator's copies) wastes scarce kernel stack.
        let mut heads = [None; INDEX_COUNT];
        let mut demand = Demand::default();
        let mut fair_min_pass = current
            .filter(|c| c.parameters.policy() == SchedulingPolicy::Fair)
            .map(|c| c.fair_pass);
        if let Some(candidate) = current {
            add_demand(&mut demand, candidate);
        }
        for (index, tree) in self.indices.iter().enumerate() {
            let Some(id) = tree.first(&mut IndexNodes(threads))? else {
                continue;
            };
            let links = threads.queue_links(id)?;
            if ready_cpu(links.membership) != Some(cpu) {
                return Err(Error::QueueCorrupted);
            }
            threads.refresh_runtime(id, now)?;
            let snapshot = threads.snapshot(id)?;
            let candidate = snapshot.candidate();
            add_demand(&mut demand, candidate);
            if candidate.parameters.policy() == SchedulingPolicy::Fair {
                fair_min_pass = Some(
                    fair_min_pass.map_or(candidate.fair_pass, |old| old.min(candidate.fair_pass)),
                );
            }
            heads[index] = Some(id);
        }
        let mut ready: Option<ReadyThread> = None;
        for id in heads.iter().flatten().copied() {
            // The same CPU lock owns both passes. No queue, runtime or policy
            // can change while demand is assembled and these heads are read.
            let candidate = threads.snapshot(id)?.candidate();
            let Some((key, _)) = window.candidate_key(now, candidate, demand) else {
                continue;
            };
            if ready.is_none_or(|old| (key, id.get()) < (old.key, old.id.get())) {
                ready = Some(ReadyThread {
                    id,
                    policy: candidate.parameters.policy(),
                    key,
                });
            }
        }
        Ok(Selection {
            ready,
            demand,
            fair_min_pass,
        })
    }

    pub fn enqueue(
        &mut self,
        threads: &mut impl ReadyQueueAccess,
        id: ThreadId,
        cpu: CpuIndex,
    ) -> Result<(), Error> {
        self.insert(threads, id, cpu, false)
    }
    pub fn enqueue_front(
        &mut self,
        threads: &mut impl ReadyQueueAccess,
        id: ThreadId,
        cpu: CpuIndex,
    ) -> Result<(), Error> {
        self.insert(threads, id, cpu, true)
    }
    fn insert(
        &mut self,
        threads: &mut impl ReadyQueueAccess,
        id: ThreadId,
        cpu: CpuIndex,
        front: bool,
    ) -> Result<(), Error> {
        let snapshot = threads.snapshot(id)?;
        let index = index_for(snapshot)?;
        match snapshot.parameters.policy() {
            SchedulingPolicy::Fifo { priority } => {
                if front {
                    self.realtime
                        .enqueue_front(threads, id, cpu, priority.get())?;
                } else {
                    self.realtime.enqueue(threads, id, cpu, priority.get())?;
                }
            }
            SchedulingPolicy::Fair | SchedulingPolicy::Budget => {
                if front {
                    self.fair.enqueue_front(threads, id, cpu)?;
                } else {
                    self.fair.enqueue(threads, id, cpu)?;
                }
            }
            SchedulingPolicy::Idle => return Err(Error::InvalidThreadState),
        }
        self.indices[index]
            .insert(&mut IndexNodes(threads), id, &mut self.scratch)
            .unwrap_or_else(|_| queue_invariant());
        if snapshot.parameters.policy() == SchedulingPolicy::Budget {
            self.budget_threads += 1;
        }
        Ok(())
    }

    /// Removes the already selected identity; never reselect across a period
    /// boundary between the scheduling decision and ownership transfer.
    pub fn remove(
        &mut self,
        threads: &mut impl ReadyQueueAccess,
        id: ThreadId,
        cpu: CpuIndex,
        membership: QueueMembership,
    ) -> Result<(), Error> {
        let snapshot = threads.snapshot(id)?;
        let mut index = index_for(snapshot)?;
        if !self.indices[index].contains(&mut IndexNodes(threads), id)? {
            if snapshot.parameters.policy() != SchedulingPolicy::Budget {
                return Err(Error::QueueCorrupted);
            }
            index = if index < BUDGET_EXHAUSTED {
                index + ROLE_COUNT
            } else {
                index - ROLE_COUNT
            };
            if !self.indices[index].contains(&mut IndexNodes(threads), id)? {
                return Err(Error::QueueCorrupted);
            }
        }
        match membership {
            QueueMembership::ReadyRealTime {
                cpu: member_cpu,
                priority,
            } if member_cpu == cpu => {
                self.realtime.remove(threads, id, cpu, priority)?;
            }
            QueueMembership::ReadyFair { cpu: member_cpu } if member_cpu == cpu => {
                self.fair.remove(threads, id, cpu)?;
            }
            _ => return Err(Error::QueueCorrupted),
        }
        self.indices[index]
            .remove(&mut IndexNodes(threads), id, &mut self.scratch)
            .unwrap_or_else(|_| queue_invariant());
        if snapshot.parameters.policy() == SchedulingPolicy::Budget {
            self.budget_threads -= 1;
        }
        Ok(())
    }
}

fn role_index(role: SchedulingRole) -> usize {
    match role {
        SchedulingRole::Ordinary => 0,
        SchedulingRole::Priority => 1,
        SchedulingRole::Protected => 2,
    }
}

fn index_for(snapshot: ReadySnapshot) -> Result<usize, Error> {
    let role = role_index(snapshot.parameters.role());
    match snapshot.parameters.policy() {
        SchedulingPolicy::Fifo { .. } => Ok(role),
        SchedulingPolicy::Fair => Ok(FAIR + role),
        SchedulingPolicy::Budget => Ok(if snapshot.budget_available() {
            BUDGET_READY + role
        } else {
            BUDGET_EXHAUSTED + role
        }),
        SchedulingPolicy::Idle => Err(Error::InvalidThreadState),
    }
}

fn add_demand(demand: &mut Demand, candidate: Candidate) {
    match candidate.parameters.policy() {
        SchedulingPolicy::Idle => return,
        SchedulingPolicy::Budget => {
            demand.any_budget = true;
            demand.eligible_budget |= candidate.budget_available;
        }
        _ => demand.fair = true,
    }
    demand.window_sensitive |= candidate.parameters.role() == SchedulingRole::Priority
        || candidate.parameters.policy() == SchedulingPolicy::Fair;
    demand.protected |= candidate.parameters.role() == SchedulingRole::Protected
        && (candidate.parameters.policy() != SchedulingPolicy::Budget
            || candidate.budget_available);
}

#[derive(Clone, Copy)]
pub(super) struct ReadySnapshot {
    parameters: SchedulingParameters,
    runtime: PolicyRuntime,
    turn: u64,
}
impl ReadySnapshot {
    fn budget_available(self) -> bool {
        matches!(self.runtime, PolicyRuntime::Budget(runtime) if runtime.remaining_ns() != 0)
    }
    fn candidate(self) -> Candidate {
        Candidate {
            parameters: self.parameters,
            fair_pass: self.runtime.candidate_key(self.parameters, self.turn).0,
            turn: self.turn,
            budget_available: self.budget_available(),
        }
    }
    fn from_schedule(schedule: &crate::task::thread::ThreadScheduleState) -> Self {
        Self {
            parameters: schedule.parameters,
            runtime: schedule.runtime,
            turn: schedule.ready_turn,
        }
    }
}

struct IndexNodes<'a, T>(&'a mut T);

type ReadyIndexKey = (u128, u8, u64, u64);

fn ready_index_key(
    schedule: &crate::task::thread::ThreadScheduleState,
    id: ThreadId,
) -> ReadyIndexKey {
    let algorithm = match schedule.parameters.policy() {
        SchedulingPolicy::Fifo { priority } => u128::from(priority.get()),
        SchedulingPolicy::Fair if schedule.parameters.fair_backend() == FairBackend::Stride => {
            schedule
                .runtime
                .candidate_key(schedule.parameters, schedule.ready_turn)
                .0
        }
        _ => u128::from(schedule.ready_turn),
    };
    (
        algorithm,
        schedule.parameters.band().get(),
        schedule.ready_turn,
        id.get(),
    )
}

impl<T: ReadyQueueAccess> Nodes for IndexNodes<'_, T> {
    type Id = ThreadId;
    type Key = ReadyIndexKey;
    type Error = Error;
    fn key(&mut self, id: ThreadId) -> Result<Self::Key, Error> {
        self.0.tree_key(id)
    }
    fn links(&mut self, id: ThreadId) -> Result<Links<ThreadId>, Error> {
        let links = self.0.tree_links(id)?;
        Ok(Links {
            left: links.left,
            right: links.right,
            height: links.height,
        })
    }
    fn set_links(&mut self, id: ThreadId, links: Links<ThreadId>) -> Result<(), Error> {
        self.0.set_tree_links(
            id,
            ReadyTreeLinks {
                left: links.left,
                right: links.right,
                height: links.height,
            },
        )
    }
    fn corrupt(&self) -> Error {
        Error::QueueCorrupted
    }
}

/// Residence list shared by Fair and Budget; dispatch ordering lives in indices.
struct FairQueue {
    queue: ThreadQueue,
}

impl FairQueue {
    const fn new() -> Self {
        Self {
            queue: ThreadQueue::new(),
        }
    }

    const fn len(&self) -> usize {
        self.queue.len
    }

    fn enqueue(
        &mut self,
        threads: &mut impl ReadyQueueAccess,
        id: ThreadId,
        cpu: CpuIndex,
    ) -> Result<(), Error> {
        push(
            threads,
            &mut self.queue,
            id,
            QueueMembership::ReadyFair { cpu },
        )?;
        Ok(())
    }

    fn enqueue_front(
        &mut self,
        threads: &mut impl ReadyQueueAccess,
        id: ThreadId,
        cpu: CpuIndex,
    ) -> Result<(), Error> {
        push_front(
            threads,
            &mut self.queue,
            id,
            QueueMembership::ReadyFair { cpu },
        )?;
        Ok(())
    }

    fn remove(
        &mut self,
        threads: &mut impl ReadyQueueAccess,
        id: ThreadId,
        cpu: CpuIndex,
    ) -> Result<(), Error> {
        remove(
            threads,
            &mut self.queue,
            id,
            QueueMembership::ReadyFair { cpu },
        )
    }
}

struct FixedPriorityFifo {
    queues: [ThreadQueue; PRIORITY_LEVELS],
    len: usize,
}

impl FixedPriorityFifo {
    pub const fn new() -> Self {
        Self {
            queues: [ThreadQueue::new(); PRIORITY_LEVELS],
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub fn enqueue(
        &mut self,
        threads: &mut impl ReadyQueueAccess,
        id: ThreadId,
        cpu: CpuIndex,
        priority: u8,
    ) -> Result<(), Error> {
        let membership = QueueMembership::ReadyRealTime { cpu, priority };
        let new_len = self.len.checked_add(1).ok_or(Error::QueueCorrupted)?;
        let queue = self
            .queues
            .get_mut(usize::from(priority))
            .ok_or(Error::QueueCorrupted)?;
        push(threads, queue, id, membership)?;
        self.len = new_len;
        Ok(())
    }

    fn enqueue_front(
        &mut self,
        threads: &mut impl ReadyQueueAccess,
        id: ThreadId,
        cpu: CpuIndex,
        priority: u8,
    ) -> Result<(), Error> {
        let membership = QueueMembership::ReadyRealTime { cpu, priority };
        let new_len = self.len.checked_add(1).ok_or(Error::QueueCorrupted)?;
        let queue = self
            .queues
            .get_mut(usize::from(priority))
            .ok_or(Error::QueueCorrupted)?;
        push_front(threads, queue, id, membership)?;
        self.len = new_len;
        Ok(())
    }

    pub fn remove(
        &mut self,
        threads: &mut impl ReadyQueueAccess,
        id: ThreadId,
        cpu: CpuIndex,
        priority: u8,
    ) -> Result<(), Error> {
        let new_len = self.len.checked_sub(1).ok_or(Error::QueueCorrupted)?;
        let queue = self
            .queues
            .get_mut(usize::from(priority))
            .ok_or(Error::QueueCorrupted)?;
        remove(
            threads,
            queue,
            id,
            QueueMembership::ReadyRealTime { cpu, priority },
        )?;
        self.len = new_len;
        Ok(())
    }
}

fn push_front(
    threads: &mut impl ThreadQueueAuthority,
    queue: &mut ThreadQueue,
    id: ThreadId,
    membership: QueueMembership,
) -> Result<(), Error> {
    let new_len = preflight_insert(threads, queue, id, membership)?;
    let old_head = queue.head;
    if let Some(head) = queue.head {
        let mut links = queue_links(threads, head).unwrap_or_else(|_| queue_invariant());
        links.previous = Some(id);
        if !threads.set_queue_links(head, links) {
            queue_invariant();
        }
    } else {
        queue.tail = Some(id);
    }
    commit_insert(
        threads,
        id,
        QueueLinks {
            previous: None,
            next: old_head,
            membership,
        },
    );
    queue.head = Some(id);
    queue.len = new_len;
    Ok(())
}

fn push(
    threads: &mut impl ThreadQueueAuthority,
    queue: &mut ThreadQueue,
    id: ThreadId,
    membership: QueueMembership,
) -> Result<(), Error> {
    let new_len = preflight_insert(threads, queue, id, membership)?;
    let old_tail = queue.tail;
    if let Some(tail) = queue.tail {
        let mut links = queue_links(threads, tail).unwrap_or_else(|_| queue_invariant());
        links.next = Some(id);
        if !threads.set_queue_links(tail, links) {
            queue_invariant();
        }
    } else {
        queue.head = Some(id);
    }
    commit_insert(
        threads,
        id,
        QueueLinks {
            previous: old_tail,
            next: None,
            membership,
        },
    );
    queue.tail = Some(id);
    queue.len = new_len;
    Ok(())
}

fn remove(
    threads: &mut impl ThreadQueueAuthority,
    queue: &mut ThreadQueue,
    id: ThreadId,
    membership: QueueMembership,
) -> Result<(), Error> {
    let links = queue_links(threads, id)?;
    if links.membership != membership || queue.len == 0 {
        return Err(Error::QueueCorrupted);
    }
    let new_len = queue.len.checked_sub(1).ok_or(Error::QueueCorrupted)?;
    validate_neighbors(threads, queue, links, id)?;
    preflight_residence(threads, id, membership)?;

    // Commit contains no fallible operation. Every involved identity and
    // reciprocal link was validated above while the transition lock retains
    // the sole table authority.
    update_previous_link(threads, queue, links);
    update_next_link(threads, queue, links);
    queue.len = new_len;
    commit_remove(threads, id, membership);
    Ok(())
}

fn update_previous_link(
    threads: &mut impl ThreadQueueAuthority,
    queue: &mut ThreadQueue,
    links: QueueLinks,
) {
    match links.previous {
        Some(previous) => {
            let mut previous_links =
                queue_links(threads, previous).unwrap_or_else(|_| queue_invariant());
            previous_links.next = links.next;
            if !threads.set_queue_links(previous, previous_links) {
                queue_invariant();
            }
        }
        None => queue.head = links.next,
    }
}

fn update_next_link(
    threads: &mut impl ThreadQueueAuthority,
    queue: &mut ThreadQueue,
    links: QueueLinks,
) {
    match links.next {
        Some(next) => {
            let mut next_links = queue_links(threads, next).unwrap_or_else(|_| queue_invariant());
            next_links.previous = links.previous;
            if !threads.set_queue_links(next, next_links) {
                queue_invariant();
            }
        }
        None => queue.tail = links.previous,
    }
}

fn validate_neighbors(
    threads: &impl ThreadReadAuthority,
    queue: &ThreadQueue,
    links: QueueLinks,
    id: ThreadId,
) -> Result<(), Error> {
    let previous_valid = match links.previous {
        Some(previous) => queue_links(threads, previous)?.next == Some(id),
        None => queue.head == Some(id),
    };
    let next_valid = match links.next {
        Some(next) => queue_links(threads, next)?.previous == Some(id),
        None => queue.tail == Some(id),
    };
    if previous_valid && next_valid {
        Ok(())
    } else {
        Err(Error::QueueCorrupted)
    }
}

fn preflight_insert(
    threads: &impl ThreadQueueAuthority,
    queue: &ThreadQueue,
    id: ThreadId,
    membership: QueueMembership,
) -> Result<usize, Error> {
    let (links, assigned_cpu) = threads.insertion_state(id, membership)?;
    if links.membership != QueueMembership::None {
        return Err(Error::ThreadAlreadyQueued);
    }
    if let Some(cpu) = ready_cpu(membership)
        && assigned_cpu != Some(cpu)
    {
        return Err(Error::InvalidThreadState);
    }
    let new_len = queue.len.checked_add(1).ok_or(Error::QueueCorrupted)?;
    match (queue.head, queue.tail, queue.len) {
        (None, None, 0) => Ok(new_len),
        (Some(head), Some(tail), len) if len != 0 => {
            let head_links = queue_links(threads, head)?;
            let tail_links = queue_links(threads, tail)?;
            if head_links.membership == membership
                && head_links.previous.is_none()
                && tail_links.membership == membership
                && tail_links.next.is_none()
            {
                Ok(new_len)
            } else {
                Err(Error::QueueCorrupted)
            }
        }
        _ => Err(Error::QueueCorrupted),
    }
}

fn preflight_residence(
    threads: &impl ThreadQueueAuthority,
    id: ThreadId,
    membership: QueueMembership,
) -> Result<(), Error> {
    let valid = threads.residence_matches(id, ready_cpu(membership))?;
    if valid {
        Ok(())
    } else {
        Err(Error::QueueCorrupted)
    }
}

fn commit_insert(threads: &mut impl ThreadQueueAuthority, id: ThreadId, links: QueueLinks) {
    if !threads.commit_insert(id, links) {
        queue_invariant();
    }
}

fn commit_remove(
    threads: &mut impl ThreadQueueAuthority,
    id: ThreadId,
    membership: QueueMembership,
) {
    if !threads.commit_remove(id, membership) {
        queue_invariant();
    }
}

const fn ready_cpu(membership: QueueMembership) -> Option<CpuIndex> {
    match membership {
        QueueMembership::ReadyRealTime { cpu, .. } | QueueMembership::ReadyFair { cpu } => {
            Some(cpu)
        }
        QueueMembership::None | QueueMembership::Waiting { .. } | QueueMembership::Terminated => {
            None
        }
    }
}

fn queue_links(threads: &impl ThreadReadAuthority, id: ThreadId) -> Result<QueueLinks, Error> {
    threads.queue_links(id)
}

pub(super) trait ThreadReadAuthority {
    fn queue_links(&self, id: ThreadId) -> Result<QueueLinks, Error>;
}

pub(super) trait ThreadQueueAuthority: ThreadReadAuthority {
    fn insertion_state(
        &self,
        id: ThreadId,
        membership: QueueMembership,
    ) -> Result<(QueueLinks, Option<CpuIndex>), Error>;
    fn residence_matches(&self, id: ThreadId, cpu: Option<CpuIndex>) -> Result<bool, Error>;
    fn set_queue_links(&mut self, id: ThreadId, links: QueueLinks) -> bool;
    fn commit_insert(&mut self, id: ThreadId, links: QueueLinks) -> bool;
    fn commit_remove(&mut self, id: ThreadId, membership: QueueMembership) -> bool;
}

pub(super) trait ReadyQueueAccess: ThreadQueueAuthority {
    fn snapshot(&mut self, id: ThreadId) -> Result<ReadySnapshot, Error>;
    fn tree_key(&mut self, id: ThreadId) -> Result<ReadyIndexKey, Error>;
    fn tree_links(&mut self, id: ThreadId) -> Result<ReadyTreeLinks, Error>;
    fn set_tree_links(&mut self, id: ThreadId, links: ReadyTreeLinks) -> Result<(), Error>;
    fn refresh_runtime(&mut self, id: ThreadId, now: u64) -> Result<(), Error>;
}

impl ThreadReadAuthority for ControlQueueAuthority<'_> {
    fn queue_links(&self, id: ThreadId) -> Result<QueueLinks, Error> {
        self.control.links(id)
    }
}

impl ThreadQueueAuthority for ControlQueueAuthority<'_> {
    fn insertion_state(
        &self,
        id: ThreadId,
        membership: QueueMembership,
    ) -> Result<(QueueLinks, Option<CpuIndex>), Error> {
        if !matches!(
            membership,
            QueueMembership::Waiting { .. } | QueueMembership::Terminated
        ) {
            return Err(Error::InvalidThreadState);
        }
        self.control.links(id).map(|links| (links, None))
    }

    fn residence_matches(&self, id: ThreadId, cpu: Option<CpuIndex>) -> Result<bool, Error> {
        if cpu.is_some() {
            return Ok(false);
        }
        self.control.links(id).map(|_links| true)
    }

    fn set_queue_links(&mut self, id: ThreadId, links: QueueLinks) -> bool {
        self.control
            .with_links_mut(id, |stored| *stored = links)
            .is_ok()
    }

    fn commit_insert(&mut self, id: ThreadId, links: QueueLinks) -> bool {
        self.control
            .with_links_mut(id, |stored| {
                if ready_cpu(links.membership).is_some()
                    || stored.membership != QueueMembership::None
                {
                    return false;
                }
                *stored = links;
                true
            })
            .unwrap_or(false)
    }

    fn commit_remove(&mut self, id: ThreadId, membership: QueueMembership) -> bool {
        self.control
            .with_links_mut(id, |stored| {
                if ready_cpu(membership).is_some() || stored.membership != membership {
                    return false;
                }
                *stored = QueueLinks::EMPTY;
                true
            })
            .unwrap_or(false)
    }
}

pub(super) fn control_push(
    threads: &mut ControlQueueAuthority<'_>,
    queue: &mut ThreadQueue,
    id: ThreadId,
    membership: QueueMembership,
) -> Result<(), Error> {
    if ready_cpu(membership).is_some() || membership == QueueMembership::None {
        return Err(Error::InvalidThreadState);
    }
    push(threads, queue, id, membership)
}

pub(super) fn control_remove(
    threads: &mut ControlQueueAuthority<'_>,
    queue: &mut ThreadQueue,
    id: ThreadId,
    membership: QueueMembership,
) -> Result<(), Error> {
    if ready_cpu(membership).is_some() || membership == QueueMembership::None {
        return Err(Error::InvalidThreadState);
    }
    remove(threads, queue, id, membership)
}

impl ThreadReadAuthority for ReadyQueueAuthority<'_, '_> {
    fn queue_links(&self, id: ThreadId) -> Result<QueueLinks, Error> {
        let owner = self
            .coordinator
            .with_thread(id, |thread| thread.schedule_owner_cpu())?;
        match owner {
            None => self.coordinator.with_thread(id, |thread| {
                thread
                    .table_owned_ready_queue_links()
                    .unwrap_or_else(|| queue_invariant())
            }),
            Some(cpu) if cpu == self.cpu.cpu() => self
                .cpu
                .with_thread(id, |_thread, schedule| schedule.ready_queue_links),
            Some(_) => Err(Error::InvalidThreadState),
        }
    }
}

impl ThreadQueueAuthority for ReadyQueueAuthority<'_, '_> {
    fn insertion_state(
        &self,
        id: ThreadId,
        membership: QueueMembership,
    ) -> Result<(QueueLinks, Option<CpuIndex>), Error> {
        let target = ready_cpu(membership).ok_or(Error::InvalidThreadState)?;
        if self.coordinator.control_links(id)?.membership != QueueMembership::None {
            return Err(Error::ThreadAlreadyQueued);
        }
        let owner = self
            .coordinator
            .with_thread(id, |thread| thread.schedule_owner_cpu())?;
        match owner {
            None => self.coordinator.with_thread(id, |thread| {
                (
                    thread
                        .table_owned_ready_queue_links()
                        .unwrap_or_else(|| queue_invariant()),
                    Some(thread.cpu_index()),
                )
            }),
            Some(cpu) if cpu == self.cpu.cpu() && target == cpu => {
                self.cpu.with_thread(id, |_thread, schedule| {
                    (
                        schedule.ready_queue_links,
                        Some(schedule.placement.assigned_cpu()),
                    )
                })
            }
            Some(_) => Err(Error::InvalidThreadState),
        }
    }

    fn residence_matches(&self, id: ThreadId, cpu: Option<CpuIndex>) -> Result<bool, Error> {
        let Some(expected) = cpu else {
            return Ok(false);
        };
        self.coordinator
            .with_thread(id, |thread| thread.schedule_owner_cpu() == Some(expected))
    }

    fn set_queue_links(&mut self, id: ThreadId, links: QueueLinks) -> bool {
        if ready_cpu(links.membership).is_none() && links != QueueLinks::EMPTY {
            return false;
        }
        self.with_schedule_mut(id, |schedule| schedule.ready_queue_links = links)
            .is_ok()
    }

    fn commit_insert(&mut self, id: ThreadId, links: QueueLinks) -> bool {
        let Some(target_cpu) = ready_cpu(links.membership) else {
            return false;
        };
        let owner = self
            .coordinator
            .with_thread(id, |thread| thread.schedule_owner_cpu());
        match owner {
            Ok(None) => self
                .coordinator
                .with_thread_mut(id, |thread| {
                    thread.publish_ready_ownership(target_cpu, links)
                })
                .unwrap_or(false),
            Ok(Some(owner)) if owner == self.cpu.cpu() && owner == target_cpu => self
                .with_schedule_mut(id, |schedule| {
                    if schedule.ready_queue_links.membership != QueueMembership::None {
                        return false;
                    }
                    schedule.ready_queue_links = links;
                    schedule.state = crate::task::thread::ThreadState::Ready;
                    true
                })
                .unwrap_or(false),
            Ok(Some(_)) | Err(_) => false,
        }
    }

    fn commit_remove(&mut self, id: ThreadId, membership: QueueMembership) -> bool {
        if ready_cpu(membership) != Some(self.cpu.cpu()) {
            return false;
        }
        self.with_schedule_mut(id, |schedule| {
            if schedule.ready_queue_links.membership != membership {
                false
            } else {
                schedule.ready_queue_links = QueueLinks::EMPTY;
                true
            }
        })
        .unwrap_or(false)
    }
}

impl ReadyQueueAccess for ReadyQueueAuthority<'_, '_> {
    fn snapshot(&mut self, id: ThreadId) -> Result<ReadySnapshot, Error> {
        self.with_schedule_mut(id, |schedule| ReadySnapshot::from_schedule(schedule))
    }
    fn tree_key(&mut self, id: ThreadId) -> Result<ReadyIndexKey, Error> {
        self.with_schedule_mut(id, |schedule| ready_index_key(schedule, id))
    }
    fn tree_links(&mut self, id: ThreadId) -> Result<ReadyTreeLinks, Error> {
        self.with_schedule_mut(id, |schedule| schedule.ready_tree)
    }
    fn set_tree_links(&mut self, id: ThreadId, links: ReadyTreeLinks) -> Result<(), Error> {
        self.with_schedule_mut(id, |schedule| schedule.ready_tree = links)
    }
    fn refresh_runtime(&mut self, id: ThreadId, now: u64) -> Result<(), Error> {
        self.with_schedule_mut(id, |schedule| {
            schedule.runtime.epoch_refresh(now, schedule.parameters)
        })
    }
}

impl ThreadReadAuthority for LocalReadyQueueAuthority<'_> {
    fn queue_links(&self, id: ThreadId) -> Result<QueueLinks, Error> {
        self.cpu
            .with_thread(id, |_thread, schedule| schedule.ready_queue_links)
    }
}

impl ThreadQueueAuthority for LocalReadyQueueAuthority<'_> {
    fn insertion_state(
        &self,
        id: ThreadId,
        membership: QueueMembership,
    ) -> Result<(QueueLinks, Option<CpuIndex>), Error> {
        let target = ready_cpu(membership).ok_or(Error::InvalidThreadState)?;
        if target != self.cpu.cpu() {
            return Err(Error::InvalidThreadState);
        }
        self.cpu.with_thread(id, |_thread, schedule| {
            (
                schedule.ready_queue_links,
                Some(schedule.placement.assigned_cpu()),
            )
        })
    }

    fn residence_matches(&self, id: ThreadId, cpu: Option<CpuIndex>) -> Result<bool, Error> {
        if cpu != Some(self.cpu.cpu()) {
            return Ok(false);
        }
        self.cpu.with_thread(id, |_thread, _schedule| true)
    }

    fn set_queue_links(&mut self, id: ThreadId, links: QueueLinks) -> bool {
        if ready_cpu(links.membership).is_none() && links != QueueLinks::EMPTY {
            return false;
        }
        self.cpu
            .with_thread_mut(id, |_thread, schedule| schedule.ready_queue_links = links)
            .is_ok()
    }

    fn commit_insert(&mut self, id: ThreadId, links: QueueLinks) -> bool {
        if ready_cpu(links.membership) != Some(self.cpu.cpu()) {
            return false;
        }
        self.cpu
            .with_thread_mut(id, |_thread, schedule| {
                if schedule.ready_queue_links.membership != QueueMembership::None {
                    return false;
                }
                schedule.ready_queue_links = links;
                schedule.state = crate::task::thread::ThreadState::Ready;
                true
            })
            .unwrap_or(false)
    }

    fn commit_remove(&mut self, id: ThreadId, membership: QueueMembership) -> bool {
        if ready_cpu(membership) != Some(self.cpu.cpu()) {
            return false;
        }
        self.cpu
            .with_thread_mut(id, |_thread, schedule| {
                if schedule.ready_queue_links.membership != membership {
                    return false;
                }
                schedule.ready_queue_links = QueueLinks::EMPTY;
                true
            })
            .unwrap_or(false)
    }
}

impl ReadyQueueAccess for LocalReadyQueueAuthority<'_> {
    fn snapshot(&mut self, id: ThreadId) -> Result<ReadySnapshot, Error> {
        self.cpu.with_thread(id, |_thread, schedule| {
            ReadySnapshot::from_schedule(schedule)
        })
    }
    fn tree_key(&mut self, id: ThreadId) -> Result<ReadyIndexKey, Error> {
        self.cpu
            .with_thread(id, |_thread, schedule| ready_index_key(schedule, id))
    }
    fn tree_links(&mut self, id: ThreadId) -> Result<ReadyTreeLinks, Error> {
        self.cpu
            .with_thread(id, |_thread, schedule| schedule.ready_tree)
    }
    fn set_tree_links(&mut self, id: ThreadId, links: ReadyTreeLinks) -> Result<(), Error> {
        self.cpu
            .with_thread_mut(id, |_thread, schedule| schedule.ready_tree = links)
    }
    fn refresh_runtime(&mut self, id: ThreadId, now: u64) -> Result<(), Error> {
        self.cpu.with_thread_mut(id, |_thread, schedule| {
            schedule.runtime.epoch_refresh(now, schedule.parameters)
        })
    }
}

fn queue_invariant() -> ! {
    hyper::debug::invariant_failure("task::scheduler::queue::queue_invariant invariant")
}
