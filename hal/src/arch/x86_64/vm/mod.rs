// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime selection between the Intel VMX and AMD SVM backends.

pub(crate) mod admission;
pub(crate) mod interrupt;
pub(crate) mod stage2;
pub(crate) mod svm;
pub(crate) mod vcpu;
pub(crate) mod vmx;

use hyper::sync::atomic::{AtomicU8, Ordering};

use crate::arch::x86_64::context::VcpuContext;

const NONE: u8 = 0;
const VMX: u8 = 1;
const SVM: u8 = 2;

static BACKEND: AtomicU8 = AtomicU8::new(NONE);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Backend {
    Vmx,
    Svm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Stage2Format {
    Ept,
    Npt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Stage2InvalidationError {
    Vmx(crate::arch::x86_64::vm::vmx::Error),
}

impl Backend {
    pub(super) const fn stage2_format(self) -> Stage2Format {
        match self {
            Self::Vmx => Stage2Format::Ept,
            Self::Svm => Stage2Format::Npt,
        }
    }

    pub(super) fn activate_stage2(self, root: u64) {
        match self {
            Self::Vmx => crate::arch::x86_64::vm::vmx::activate_ept(root),
            Self::Svm => crate::arch::x86_64::vm::svm::activate_npt(root),
        }
    }

    pub(super) fn invalidate_stage2(self, root: u64) -> Result<(), Stage2InvalidationError> {
        match self {
            Self::Vmx => crate::arch::x86_64::vm::vmx::invalidate_ept(root)
                .map_err(Stage2InvalidationError::Vmx),
            Self::Svm => {
                crate::arch::x86_64::vm::svm::invalidate_npt();
                Ok(())
            }
        }
    }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(super) fn validate() -> Result<Backend, crate::arch::x86_64::vm::admission::ValidationError> {
    if let Some(backend) = selected() {
        return Ok(backend);
    }
    let basic = core::arch::x86_64::__cpuid(1);
    let extended_max = core::arch::x86_64::__cpuid(0x8000_0000).eax;
    let extended = (extended_max >= 0x8000_0001).then(|| core::arch::x86_64::__cpuid(0x8000_0001));
    let backend = if basic.ecx & (1 << 5) != 0 {
        crate::arch::x86_64::vm::vmx::validate()?;
        Backend::Vmx
    } else if extended.is_some_and(|features| features.ecx & (1 << 2) != 0) {
        crate::arch::x86_64::vm::svm::validate()?;
        Backend::Svm
    } else {
        return Err(crate::arch::x86_64::vm::admission::ValidationError::HardwareUnavailable);
    };
    let encoded = encode(backend);
    match BACKEND.compare_exchange(NONE, encoded, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(backend),
        Err(value) if value == encoded => Ok(backend),
        Err(_) => Err(crate::arch::x86_64::vm::admission::ValidationError::BackendConflict),
    }
}

pub(super) fn selected() -> Option<Backend> {
    match BACKEND.load(Ordering::Acquire) {
        VMX => Some(Backend::Vmx),
        SVM => Some(Backend::Svm),
        _ => None,
    }
}

pub(super) fn observe_host_interrupt(vector: u32) {
    if selected() == Some(Backend::Svm) {
        crate::arch::x86_64::vm::svm::observe_host_interrupt(vector);
    }
}

pub(super) unsafe fn enter(context: *mut VcpuContext) -> ! {
    match selected() {
        // SAFETY: The caller's raw-context contract is forwarded to the selected backend.
        Some(Backend::Vmx) => unsafe { crate::arch::x86_64::vm::vmx::enter(context) },
        // SAFETY: The caller's raw-context contract is forwarded to the selected backend.
        Some(Backend::Svm) => unsafe { crate::arch::x86_64::vm::svm::enter(context) },
        None => fail_stop("x86 virtualization backend selection", NONE),
    }
}

/// Enters registered kernel fail-stop policy for an unrecoverable backend error.
///
/// Runtime exception services are published before VM initialization, and a
/// vCPU cannot be constructed without the later VM-entry readiness capability.
/// Capturing the machine context here keeps backend diagnostics below the
/// registered architecture-entry boundary.
#[cold]
#[inline(never)]
pub(super) fn fail_stop(operation: &str, detail: impl core::fmt::Debug) -> ! {
    crate::arch::exception::fatal(
        crate::arch::exception::capture_crash_context(),
        format_args!("fatal x86 virtualization failure: {operation}: {detail:?}"),
    )
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
const fn encode(backend: Backend) -> u8 {
    match backend {
        Backend::Vmx => VMX,
        Backend::Svm => SVM,
    }
}
