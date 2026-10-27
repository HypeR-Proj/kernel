// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected host-interrupt capabilities.
//!
//! Kernel policy owns IRQ domains, handler lifetimes, routing, and failure
//! policy. This facade binds those policies to the selected architecture's
//! local-mask, controller, platform-decoding, and reschedule-notification
//! mechanisms. Guest interrupt virtualization is exposed separately by the VM
//! capability facade.

use hyper::cpu::CpuIndex;
use hyper::hal::interrupt::{InterruptId, KernelRpcReasons};
use hyper_platform::PlatformInterrupt;

pub type Controller = crate::arch::irq::Controller;
pub type ControllerError = crate::arch::irq::ControllerError;
pub type LocalMask = crate::arch::irq::LocalMask;
pub type KernelRpcServiceError = crate::arch::irq::KernelRpcServiceError;

/// Selected platform interrupt descriptor could not be decoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeError;

/// Decodes one platform interrupt descriptor for the selected controller ABI.
pub fn decode_platform(descriptor: &[u32]) -> Result<PlatformInterrupt, DecodeError> {
    crate::arch::irq::decode_platform(descriptor).map_err(|_| DecodeError)
}

/// Masks ordinary local IRQ delivery until [`enable_local`] is called.
///
/// Lexical critical sections must use [`LocalMask`] through an interrupt-mask
/// guard so the exact prior state is restored instead of enabling IRQs
/// unconditionally.
pub fn mask_local() {
    crate::arch::irq::mask_local();
}

/// Irreversibly masks every architecture-supported local interrupt source.
///
/// This is reserved for fail-stop paths. Runtime transitions must use
/// [`mask_local`] so architecture source-enable state remains intact.
pub fn disable_all_sources() {
    crate::arch::irq::disable_all_sources();
}

/// Enables ordinary local IRQ delivery after runtime vectors are installed.
pub fn enable_local() {
    crate::arch::irq::enable_local();
}

/// Reports whether ordinary local IRQ delivery is currently enabled.
pub fn local_enabled() -> bool {
    crate::arch::irq::local_enabled()
}

/// Returns the architecture-reserved physical reschedule interrupt, if any.
pub fn reschedule_interrupt() -> Option<InterruptId> {
    crate::arch::irq::reschedule_interrupt()
}

/// Prompts `cpu` to evaluate an already-published reschedule request.
///
/// `false` means the selected backend cannot issue a qualified targeted
/// notification. The caller must preserve the durable request and use its
/// architecture-neutral wake fallback.
pub fn notify_reschedule(cpu: CpuIndex) -> bool {
    crate::arch::irq::notify_reschedule(cpu)
}

pub fn kernel_rpc_interrupt() -> Option<InterruptId> {
    crate::arch::irq::kernel_rpc_interrupt()
}

pub fn arm_kernel_rpc_source() {
    crate::arch::irq::arm_kernel_rpc_source();
}

pub fn notify_kernel_rpc(cpu: CpuIndex, reasons: KernelRpcReasons) -> bool {
    crate::arch::irq::notify_kernel_rpc(cpu, reasons.bits())
}

pub fn take_kernel_rpc_reasons() -> KernelRpcReasons {
    KernelRpcReasons::from_bits(crate::arch::irq::take_kernel_rpc_reasons())
}

pub fn install_kernel_rpc_services(
    poll: fn(),
    interrupt: fn(hyper::hal::interrupt::InterruptOrigin) -> hyper::hal::interrupt::EntryAction,
) -> Result<(), KernelRpcServiceError> {
    crate::arch::irq::install_kernel_rpc_services(poll, interrupt)
}

/// Registers from the interrupted execution, copied only on explicit request.
/// This does not expose the architecture's mutable exception frame.
#[derive(Clone, Copy, Debug)]
pub struct InterruptedRegisters {
    pub pc: usize,
    pub sp: usize,
    pub fp: usize,
    pub origin: hyper::hal::interrupt::InterruptOrigin,
}

/// Read-only, CPU-affine view valid only during synchronous IRQ dispatch.
///
/// Passing this view does not read registers or consult any observer. A handler
/// may request an owned register snapshot after its IRQ route has been selected.
/// The borrow cannot escape into deferred work or survive interrupt return.
pub struct InterruptFrame<'a> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    inner: crate::arch::aarch64::InterruptFrame<'a>,
    _local: core::marker::PhantomData<&'a *mut ()>,
}

impl<'a> InterruptFrame<'a> {
    #[cfg(CONFIG_ARCH_AARCH64)]
    pub(crate) fn new(inner: crate::arch::aarch64::InterruptFrame<'a>) -> Self {
        Self {
            inner,
            _local: core::marker::PhantomData,
        }
    }

    /// Other backends do not yet supply a qualified interrupted register view.
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    pub(crate) const fn unavailable() -> Self {
        Self {
            _local: core::marker::PhantomData,
        }
    }

    /// Copies interrupted registers when this entry backend supports it.
    /// This must be called with local IRQs masked, before IRQ-tail scheduling.
    pub fn registers(&self) -> Option<InterruptedRegisters> {
        #[cfg(CONFIG_ARCH_AARCH64)]
        {
            self.inner.registers()
        }
        #[cfg(not(CONFIG_ARCH_AARCH64))]
        {
            None
        }
    }
}
