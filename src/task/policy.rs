// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Validated scheduling inputs and thread placement vocabulary.
//!
//! This module describes policy inputs without owning runnable queues or
//! performing context switches. Algorithms and arbitration parameters do not
//! depend on whether a Thread executes kernel, Native, or guest code.
//! Placement records current assignment separately from affinity and placement
//! policy so the scheduler can move a stopped Thread without changing identity.

use hyper::cpu::{CpuIndex, MAX_CPUS};

const CPUS_PER_MASK_WORD: usize = u64::BITS as usize;
const CPU_MASK_WORDS: usize = MAX_CPUS.div_ceil(CPUS_PER_MASK_WORD);

/// The scheduler accepts the complete `u8` priority namespace.
pub const PRIORITY_LEVELS: usize = u8::MAX as usize + 1;

/// Fixed scheduler priority. Lower numeric values run first.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ThreadPriority(u8);

impl ThreadPriority {
    pub const HIGHEST: Self = Self(0);
    pub const NORMAL: Self = Self(128);
    pub const LOWEST: Self = Self(u8::MAX);

    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Closed set of scheduling classes supported by this kernel image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulingClass {
    /// FIFO policy; its cross-algorithm rank comes from the arbitration role.
    RealTime,
    /// Ordinary time-sharing work.
    Fair,
    /// Fixed-window execution budgets with work-conserving idle borrowing.
    Budget,
    /// Per-CPU fallback execution which is never inserted into a run queue.
    Idle,
}

/// Class-specific scheduling parameters.
///
/// Encoding Idle as a separate variant prevents assigning a meaningless
/// fixed priority to an idle thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulingPolicy {
    /// Fixed-priority real-time FIFO scheduling.
    ///
    /// A higher-priority ready thread becomes eligible immediately, but the
    /// switch occurs only at an explicit safe point. Equal-priority wakeups do
    /// not request a switch.
    /// A priority change preserves position when unchanged, enters the tail of
    /// a raised-priority queue, and enters the head of a lowered-priority queue
    /// at the next scheduling decision.
    Fifo {
        priority: ThreadPriority,
    },
    /// Ordinary time-sharing scheduling.
    ///
    /// The selected backend and weight are separate validated parameters;
    /// changing the algorithm does not change execution or placement identity.
    Fair,
    /// A bounded execution entitlement replenished at a common wall-clock period.
    Budget,
    Idle,
}

impl SchedulingPolicy {
    pub const fn fifo(priority: ThreadPriority) -> Self {
        Self::Fifo { priority }
    }

    pub const fn fair() -> Self {
        Self::Fair
    }

    pub const fn class(self) -> SchedulingClass {
        match self {
            Self::Fifo { .. } => SchedulingClass::RealTime,
            Self::Fair => SchedulingClass::Fair,
            Self::Budget => SchedulingClass::Budget,
            Self::Idle => SchedulingClass::Idle,
        }
    }

    pub const fn priority(self) -> Option<ThreadPriority> {
        match self {
            Self::Fifo { priority } => Some(priority),
            Self::Fair | Self::Budget | Self::Idle => None,
        }
    }
}

/// Arbitration privileges, independent of algorithm and execution payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulingRole {
    Ordinary,
    /// May use the CPU's bounded priority-execution allowance.
    Priority,
    /// Participates in the CPU's protected minimum execution opportunity.
    Protected,
}

/// Fair selection backends share accounting and queue ownership contracts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FairBackend {
    Stride,
    RoundRobin,
}

/// A bounded, nonzero share within the Fair algorithm.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ThreadWeight(u16);

impl ThreadWeight {
    pub const DEFAULT: Self = Self(16);
    pub const MAX: u16 = 1024;

    pub const fn new(value: u16) -> Option<Self> {
        if value == 0 || value > Self::MAX {
            None
        } else {
            Some(Self(value))
        }
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Budget quantities are nanoseconds. A budget is not a reservation guarantee.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetParameters {
    budget_ns: u64,
    period_ns: u64,
    burst_ns: u64,
}

impl BudgetParameters {
    pub const MAX_PERIOD_NS: u64 = 1_000_000_000;

    pub const fn new(budget_ns: u64, period_ns: u64, burst_ns: u64) -> Option<Self> {
        if period_ns == 0
            || period_ns > Self::MAX_PERIOD_NS
            || budget_ns == 0
            || budget_ns > period_ns
            || burst_ns == 0
            || burst_ns > period_ns
        {
            None
        } else {
            Some(Self {
                budget_ns,
                period_ns,
                burst_ns,
            })
        }
    }

    pub const fn budget_ns(self) -> u64 {
        self.budget_ns
    }
    pub const fn period_ns(self) -> u64 {
        self.period_ns
    }
    pub const fn burst_ns(self) -> u64 {
        self.burst_ns
    }
}

/// Validated policy inputs. Rights and limits are checked before installation.
///
/// Unused fields have canonical values so equality does not hide changes in
/// parameters which could become active after a later algorithm transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulingParameters {
    policy: SchedulingPolicy,
    band: ThreadPriority,
    role: SchedulingRole,
    weight: ThreadWeight,
    quantum_ns: u64,
    fair_backend: FairBackend,
    budget: Option<BudgetParameters>,
}

impl SchedulingParameters {
    pub const fn fair(
        weight: ThreadWeight,
        quantum_ns: u64,
        band: ThreadPriority,
        role: SchedulingRole,
        fair_backend: FairBackend,
    ) -> Option<Self> {
        if quantum_ns == 0 || quantum_ns > BudgetParameters::MAX_PERIOD_NS {
            return None;
        }
        Some(Self {
            policy: SchedulingPolicy::Fair,
            band,
            role,
            weight,
            quantum_ns,
            fair_backend,
            budget: None,
        })
    }

    pub const fn budget(
        budget: BudgetParameters,
        band: ThreadPriority,
        role: SchedulingRole,
    ) -> Self {
        Self {
            policy: SchedulingPolicy::Budget,
            band,
            role,
            weight: ThreadWeight::DEFAULT,
            quantum_ns: 0,
            fair_backend: FairBackend::Stride,
            budget: Some(budget),
        }
    }

    pub const fn fifo(
        priority: ThreadPriority,
        band: ThreadPriority,
        role: SchedulingRole,
    ) -> Self {
        Self {
            policy: SchedulingPolicy::Fifo { priority },
            band,
            role,
            weight: ThreadWeight::DEFAULT,
            quantum_ns: 0,
            fair_backend: FairBackend::Stride,
            budget: None,
        }
    }

    pub const fn idle() -> Self {
        Self {
            policy: SchedulingPolicy::Idle,
            band: ThreadPriority::LOWEST,
            role: SchedulingRole::Ordinary,
            weight: ThreadWeight::DEFAULT,
            quantum_ns: 0,
            fair_backend: FairBackend::Stride,
            budget: None,
        }
    }

    pub const fn policy(self) -> SchedulingPolicy {
        self.policy
    }
    pub const fn band(self) -> ThreadPriority {
        self.band
    }
    pub const fn role(self) -> SchedulingRole {
        self.role
    }
    pub const fn weight(self) -> ThreadWeight {
        self.weight
    }
    pub const fn quantum_ns(self) -> u64 {
        self.quantum_ns
    }
    pub const fn fair_backend(self) -> FairBackend {
        self.fair_backend
    }
    pub const fn budget_parameters(self) -> Option<BudgetParameters> {
        self.budget
    }
}

/// CPUs on which a stopped thread is permitted to run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuMask {
    words: [u64; CPU_MASK_WORDS],
}

impl CpuMask {
    pub const EMPTY: Self = Self {
        words: [0; CPU_MASK_WORDS],
    };

    pub const ALL: Self = Self::all();

    pub const fn single(cpu: CpuIndex) -> Self {
        Self::EMPTY.with_cpu(cpu)
    }

    pub const fn contains(self, cpu: CpuIndex) -> bool {
        let index = cpu.get();
        self.words[index / CPUS_PER_MASK_WORD] & (1u64 << (index % CPUS_PER_MASK_WORD)) != 0
    }

    pub const fn is_empty(self) -> bool {
        let mut word = 0;
        while word < CPU_MASK_WORDS {
            if self.words[word] != 0 {
                return false;
            }
            word += 1;
        }
        true
    }

    /// Adds one allowed CPU and returns the enlarged immutable value.
    pub const fn with_cpu(mut self, cpu: CpuIndex) -> Self {
        let index = cpu.get();
        self.words[index / CPUS_PER_MASK_WORD] |= 1u64 << (index % CPUS_PER_MASK_WORD);
        self
    }

    /// Removes one allowed CPU and returns the reduced immutable value.
    pub const fn without_cpu(mut self, cpu: CpuIndex) -> Self {
        let index = cpu.get();
        self.words[index / CPUS_PER_MASK_WORD] &= !(1u64 << (index % CPUS_PER_MASK_WORD));
        self
    }

    const fn all() -> Self {
        let mut mask = Self {
            words: [u64::MAX; CPU_MASK_WORDS],
        };
        let remainder = MAX_CPUS % CPUS_PER_MASK_WORD;
        if remainder != 0 {
            mask.words[CPU_MASK_WORDS - 1] = (1u64 << remainder) - 1;
        }
        mask
    }
}

/// CPU-selection constraints retained independently of current assignment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlacementPolicy {
    Movable,
    Pinned(CpuIndex),
}

/// Scheduler assignment of a thread within its permitted CPU affinity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadPlacement {
    assigned_cpu: CpuIndex,
    affinity: CpuMask,
    policy: PlacementPolicy,
    last_cpu: Option<CpuIndex>,
}

impl ThreadPlacement {
    /// Assigns a thread to one CPU admitted by `affinity`.
    ///
    /// Constructing placement does not migrate a live Thread. Scheduler code
    /// may install a different value only under its stopped-thread handoff.
    #[cfg(test)]
    pub const fn new(
        assigned_cpu: CpuIndex,
        affinity: CpuMask,
        policy: PlacementPolicy,
    ) -> Option<Self> {
        let policy_valid = match policy {
            PlacementPolicy::Movable => true,
            PlacementPolicy::Pinned(cpu) => {
                affinity.contains(cpu) && assigned_cpu.get() == cpu.get()
            }
        };
        if affinity.contains(assigned_cpu) && policy_valid {
            Some(Self {
                assigned_cpu,
                affinity,
                policy,
                last_cpu: None,
            })
        } else {
            None
        }
    }

    /// Creates the non-migrating placement used by the current scheduler.
    pub const fn pinned(cpu: CpuIndex) -> Self {
        Self {
            assigned_cpu: cpu,
            affinity: CpuMask::single(cpu),
            policy: PlacementPolicy::Pinned(cpu),
            last_cpu: Some(cpu),
        }
    }

    /// Creates an initially assigned but migration-capable placement.
    #[cfg(test)]
    pub const fn movable(cpu: CpuIndex) -> Self {
        Self {
            assigned_cpu: cpu,
            affinity: CpuMask::ALL,
            policy: PlacementPolicy::Movable,
            last_cpu: None,
        }
    }

    /// Creates a migration-capable placement constrained by `affinity`.
    pub const fn movable_with_affinity(cpu: CpuIndex, affinity: CpuMask) -> Option<Self> {
        if affinity.contains(cpu) {
            Some(Self {
                assigned_cpu: cpu,
                affinity,
                policy: PlacementPolicy::Movable,
                last_cpu: None,
            })
        } else {
            None
        }
    }

    pub const fn assigned_cpu(self) -> CpuIndex {
        self.assigned_cpu
    }

    pub const fn affinity(self) -> CpuMask {
        self.affinity
    }

    pub(crate) const fn policy(self) -> PlacementPolicy {
        self.policy
    }

    #[cfg(test)]
    pub const fn last_cpu(self) -> Option<CpuIndex> {
        self.last_cpu
    }

    pub(crate) const fn mark_running(mut self, cpu: CpuIndex) -> Option<Self> {
        if self.assigned_cpu.get() != cpu.get() || !self.affinity.contains(cpu) {
            return None;
        }
        self.last_cpu = Some(cpu);
        Some(self)
    }

    /// Replaces a movable Thread's affinity while retaining its assignment.
    pub(crate) const fn with_affinity(mut self, affinity: CpuMask) -> Option<Self> {
        if !matches!(self.policy, PlacementPolicy::Movable)
            || affinity.is_empty()
            || !affinity.contains(self.assigned_cpu)
        {
            return None;
        }
        self.affinity = affinity;
        Some(self)
    }

    /// Atomically changes affinity and stopped assignment.
    pub(crate) const fn reassign_with_affinity(
        mut self,
        cpu: CpuIndex,
        affinity: CpuMask,
    ) -> Option<Self> {
        if !matches!(self.policy, PlacementPolicy::Movable)
            || affinity.is_empty()
            || !affinity.contains(cpu)
        {
            return None;
        }
        self.assigned_cpu = cpu;
        self.affinity = affinity;
        Some(self)
    }
}
