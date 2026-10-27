// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fixed per-VM admission for unhandled guest MMIO diagnostics.

use core::fmt;

use hyper::sync::atomic::{AtomicU32, Ordering};
use {hyper_vm_models::exit::MmioAccess, hyper_vm_models::exit::MmioOperation};

const DETAILED_REPORT_LIMIT: u32 = 4;
const SUPPRESSION_NOTICE_ORDINAL: u32 = DETAILED_REPORT_LIMIT + 1;

/// One admission decision from a VM-lifetime diagnostic budget.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Admission {
    Detailed { ordinal: u32 },
    SuppressionNotice { ordinal: u32 },
}

/// Diagnostic state whose lifetime and budget follow one VM aggregate.
pub(super) struct VmDiagnostics {
    unhandled_mmio: UnhandledMmioGate,
}

impl VmDiagnostics {
    pub(super) const fn new() -> Self {
        Self {
            unhandled_mmio: UnhandledMmioGate::new(),
        }
    }

    pub(super) fn admit_unhandled_mmio(
        &self,
        vm: VmDiagnosticId,
        vcpu: u32,
        access: MmioAccess,
    ) -> Option<UnhandledMmioReport> {
        self.unhandled_mmio
            .admit()
            .map(|admission| UnhandledMmioReport {
                vm,
                vcpu,
                access,
                admission,
            })
    }
}

/// Stable VM identity copied into a report before its registry binding ends.
#[derive(Clone, Copy)]
pub(super) struct VmDiagnosticId {
    slot: u32,
    generation: u32,
}

impl VmDiagnosticId {
    pub(super) const fn new(slot: u32, generation: u32) -> Self {
        Self { slot, generation }
    }
}

/// Owned report ticket that can outlive active-vCPU and device-model borrows.
#[derive(Clone, Copy)]
pub(crate) struct UnhandledMmioReport {
    vm: VmDiagnosticId,
    vcpu: u32,
    access: MmioAccess,
    admission: Admission,
}

impl fmt::Display for UnhandledMmioReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "HypeR: unhandled guest MMIO: VM {}:{}, vCPU {}, GPA {:#x}, width {:?}, ",
            self.vm.slot,
            self.vm.generation,
            self.vcpu,
            self.access.address().get(),
            self.access.width()
        )?;
        match self.access.operation() {
            MmioOperation::Read => formatter.write_str("read")?,
            MmioOperation::Write(value) => write!(formatter, "write {value:#x}")?,
        }
        match self.admission {
            Admission::Detailed { ordinal } => write!(formatter, ", occurrence {ordinal}"),
            Admission::SuppressionNotice { ordinal } => write!(
                formatter,
                ", occurrence {ordinal}; further reports for this VM are suppressed"
            ),
        }
    }
}

/// Bounds terminal MMIO reporting for one installed VM lifetime.
///
/// The counter arbitrates independent report tickets only: it does not publish
/// data consumed by an admitted reporter. Relaxed ordering is therefore
/// sufficient; the atomic modification order still grants exact, unique
/// ordinals under concurrent vCPU exits.
pub(super) struct UnhandledMmioGate {
    observed: AtomicU32,
}

impl UnhandledMmioGate {
    pub(super) const fn new() -> Self {
        Self {
            observed: AtomicU32::new(0),
        }
    }

    #[cfg(test)]
    pub(super) const fn with_observed_for_test(observed: u32) -> Self {
        Self {
            observed: AtomicU32::new(observed),
        }
    }

    pub(super) fn admit(&self) -> Option<Admission> {
        let mut observed = self.observed.load(Ordering::Relaxed);
        loop {
            let next = saturating_increment(observed);
            match self.observed.compare_exchange_weak(
                observed,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(previous) => return admission_for(previous),
                Err(current) => observed = current,
            }
        }
    }
}

const fn admission_for(previous: u32) -> Option<Admission> {
    let ordinal = saturating_increment(previous);
    if ordinal <= DETAILED_REPORT_LIMIT {
        Some(Admission::Detailed { ordinal })
    } else if ordinal == SUPPRESSION_NOTICE_ORDINAL {
        Some(Admission::SuppressionNotice { ordinal })
    } else {
        None
    }
}

pub(super) const fn saturating_increment(value: u32) -> u32 {
    value.saturating_add(1)
}

impl Default for UnhandledMmioGate {
    fn default() -> Self {
        Self::new()
    }
}
