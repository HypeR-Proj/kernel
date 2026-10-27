// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

/// Architecture policy used by locks that must be safe against local IRQs.
///
/// Implementations must restore the exact interrupt mask captured by
/// `save_and_disable`, including when interrupts were already disabled.
pub trait InterruptMask {
    type State: Copy;

    fn save_and_disable() -> Self::State;
    fn restore(state: Self::State);

    /// Makes architecture-required progress while an IRQ-safe lock is contended.
    ///
    /// Implementations must remain allocation-free and must not acquire another
    /// lock. They may enter a previously installed poll-safe kernel service only
    /// through an opaque architecture/HAL callback seam. Architectures without
    /// an urgent masked service retain the ordinary processor spin hint.
    fn wait_for_lock_owner() {
        core::hint::spin_loop();
    }
}

/// Architecture-independent hardware interrupt number.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct InterruptId(u32);

impl InterruptId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Execution world interrupted by one physical interrupt.
///
/// Architecture entry constructs this value from state which cannot be
/// reconstructed by kernel policy. In particular, `Host` and `Guest` must not
/// be inferred from the absence of a native-user unwind callback.
#[derive(Clone, Copy, Debug)]
pub enum InterruptOrigin {
    Host,
    Guest,
    Native { unwind: unsafe extern "C" fn() },
}

impl InterruptOrigin {
    pub const fn native_unwind(self) -> Option<unsafe extern "C" fn()> {
        match self {
            Self::Native { unwind } => Some(unwind),
            Self::Host | Self::Guest => None,
        }
    }

    pub const fn is_guest(self) -> bool {
        matches!(self, Self::Guest)
    }
}

/// Kernel disposition for an architecture-acknowledged physical interrupt.
#[derive(Clone, Copy, Debug)]
#[must_use]
pub enum EntryAction {
    /// Return after registered policy completed the acknowledged interrupt.
    Resume {
        /// Work which architecture entry must invoke exactly once after the
        /// registered dispatcher completed EOI/deactivation, on the
        /// interrupted Thread stack with local interrupts masked.
        postlude: Option<unsafe extern "C" fn()>,
    },
    /// A durable administrative request owns the interrupted guest and vector
    /// entry must capture it into the typed guest-run return frame.
    StopGuest {
        /// Qualified fallback for backends which stop through their ordinary
        /// IRQ-tail continuation rather than a terminal run-frame unwind.
        postlude: Option<unsafe extern "C" fn()>,
    },
    /// Registered policy completed the interrupt and selected fail-stop.
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KernelRpcReasons(u8);

impl KernelRpcReasons {
    const KNOWN_BITS: u8 = Self::LOCAL_IRQ_LIFECYCLE.0
        | Self::STAGE1_TLB_SHOOTDOWN.0
        | Self::USER_ADDRESS_SPACE.0
        | Self::GUEST_STAGE2.0
        | Self::PMU_CONTROL.0;
    pub const NONE: Self = Self(0);
    pub const LOCAL_IRQ_LIFECYCLE: Self = Self(1 << 0);
    pub const STAGE1_TLB_SHOOTDOWN: Self = Self(1 << 1);
    pub const USER_ADDRESS_SPACE: Self = Self(1 << 2);
    pub const GUEST_STAGE2: Self = Self(1 << 3);
    /// Reconcile the calling CPU with the durable profiling request. This uses
    /// the shared IPI doorbell, independently of synchronous RPC transactions.
    pub const PMU_CONTROL: Self = Self(1 << 4);

    pub const fn bits(self) -> u8 {
        self.0
    }
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }
    pub const fn contains(self, reason: Self) -> bool {
        self.0 & reason.0 != 0
    }
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub const fn has_unknown(self) -> bool {
        self.0 & !Self::KNOWN_BITS != 0
    }
}

/// Kernel IRQ-domain binding consumed by an architecture mechanism.
///
/// This is distinct from a physical [`InterruptId`] and from a guest-visible
/// interrupt number. Kernel IRQ policy creates the binding; architecture code
/// may use it only through the narrow service which received it.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HostInterruptBinding(u32);

impl HostInterruptBinding {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Electrical behavior selected for a configurable interrupt input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptTrigger {
    Level,
    Edge,
}

/// Architecture-neutral urgency assigned by kernel interrupt policy.
///
/// Controllers translate this ordering into their native representation; for
/// example, lower GIC values and higher PLIC values both mean greater urgency.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum InterruptPriority {
    Critical,
    High,
    Normal,
    Low,
}

/// Failure phase for an interrupt-controller enable or disable transition.
///
/// Register interfaces such as `GICv3` can report a completion timeout only
/// after the command write is visible. Callers must not treat that state like
/// validation failure: the route may already have changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptTransitionError<Error> {
    /// Validation failed before any enable-state register was written.
    NotApplied(Error),
    /// The command was written, but its final hardware state is not proven.
    AppliedOrUnknown(Error),
}

impl<Error> InterruptTransitionError<Error> {
    pub fn map<Mapped>(
        self,
        map: impl FnOnce(Error) -> Mapped,
    ) -> InterruptTransitionError<Mapped> {
        match self {
            Self::NotApplied(error) => InterruptTransitionError::NotApplied(map(error)),
            Self::AppliedOrUnknown(error) => InterruptTransitionError::AppliedOrUnknown(map(error)),
        }
    }
}

/// Common interrupt-controller operations used by kernel policy.
pub trait InterruptController {
    type Error;

    fn enable(
        &mut self,
        interrupt: InterruptId,
    ) -> Result<(), InterruptTransitionError<Self::Error>>;
    fn disable(
        &mut self,
        interrupt: InterruptId,
    ) -> Result<(), InterruptTransitionError<Self::Error>>;
    fn acknowledge(&self) -> Option<InterruptId>;
    fn end(&self, interrupt: InterruptId);
}

/// CPU-private interrupt-controller capability.
///
/// A value is constructed only for the executing CPU and must remain pinned
/// in that CPU's per-CPU slot. It deliberately exposes no shared Distributor
/// or routing operations.
pub trait LocalInterruptController {
    type Error;

    /// Programs a route which the caller keeps disabled.
    ///
    /// Failure may leave priority or trigger fields partially updated. Because
    /// delivery remains disabled, callers may recover by overwriting the full
    /// configuration before a later enable transition.
    fn configure(
        &self,
        interrupt: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
    ) -> Result<(), Self::Error>;
    fn enable(&self, interrupt: InterruptId) -> Result<(), InterruptTransitionError<Self::Error>>;
    fn disable(&self, interrupt: InterruptId) -> Result<(), InterruptTransitionError<Self::Error>>;
}

/// Operations required by the kernel IRQ-domain policy in addition to the
/// generic acknowledge/enable lifecycle.
pub trait KernelInterruptController: InterruptController {
    type Local: LocalInterruptController<Error = Self::Error>;

    fn interrupt_count(&self) -> u32;
    /// Programs a route which the caller keeps disabled.
    ///
    /// Unlike [`InterruptController::enable`] and
    /// [`InterruptController::disable`], a partial configuration write cannot
    /// expose a callback. The caller must overwrite the complete configuration
    /// before enabling a route whose configuration returned an error.
    fn configure(
        &mut self,
        interrupt: InterruptId,
        priority: InterruptPriority,
        trigger: InterruptTrigger,
    ) -> Result<(), Self::Error>;

    /// Returns whether state for this source must be installed on every CPU.
    fn is_per_cpu(&self, interrupt: InterruptId) -> bool;

    /// Derives a pinned capability for the already-initialized calling CPU.
    fn local_controller(&self) -> Result<Self::Local, Self::Error>;

    /// Initializes controller state private to the calling secondary CPU.
    ///
    /// # Safety
    ///
    /// Shared controller initialization must be complete and local interrupts
    /// must remain masked.
    unsafe fn initialize_local(&mut self) -> Result<Self::Local, Self::Error>;
}
