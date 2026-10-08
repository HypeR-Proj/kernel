// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[path = "../../../../src/kernel/vm/diagnostics.rs"]
mod model;

use std::sync::{Arc, Barrier};
use std::thread;

use hyper::vm::exit::{AccessWidth, GuestPhysicalAddress, MmioAccess, MmioOperation};
use model::{Admission, UnhandledMmioGate, VmDiagnosticId, VmDiagnostics};

#[test]
fn admits_exactly_four_details_and_one_suppression_notice() {
    let gate = UnhandledMmioGate::new();
    for ordinal in 1..=4 {
        assert_eq!(gate.admit(), Some(Admission::Detailed { ordinal }));
    }
    assert_eq!(
        gate.admit(),
        Some(Admission::SuppressionNotice { ordinal: 5 })
    );
    assert_eq!(gate.admit(), None);
}

#[test]
fn saturated_count_and_closed_gate_never_reopen() {
    assert_eq!(model::saturating_increment(u32::MAX), u32::MAX);

    let gate = UnhandledMmioGate::with_observed_for_test(u32::MAX - 1);
    for _ in 0..8 {
        assert_eq!(gate.admit(), None);
    }
}

#[test]
fn concurrent_observers_receive_one_exact_admission_set() {
    const OBSERVERS: usize = 32;

    let gate = Arc::new(UnhandledMmioGate::new());
    let barrier = Arc::new(Barrier::new(OBSERVERS));
    let mut workers = std::vec::Vec::new();
    for _ in 0..OBSERVERS {
        let gate = Arc::clone(&gate);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            gate.admit()
        }));
    }

    let mut details = [false; 4];
    let mut notices = 0;
    let mut suppressed = 0;
    for worker in workers {
        let admission = match worker.join() {
            Ok(admission) => admission,
            Err(_) => panic!("diagnostic observer panicked"),
        };
        match admission {
            Some(Admission::Detailed { ordinal }) => {
                let index = crate::require_some(usize::try_from(ordinal - 1).ok());
                details[index] = true;
            }
            Some(Admission::SuppressionNotice { ordinal: 5 }) => notices += 1,
            Some(Admission::SuppressionNotice { .. }) => {
                panic!("received an invalid suppression ordinal")
            }
            None => suppressed += 1,
        }
    }
    assert_eq!(details, [true; 4]);
    assert_eq!(notices, 1);
    assert_eq!(suppressed, OBSERVERS - 5);
}

#[test]
fn separate_vm_gates_have_independent_budgets() {
    let first = UnhandledMmioGate::new();
    let second = UnhandledMmioGate::new();
    for ordinal in 1..=4 {
        assert_eq!(first.admit(), Some(Admission::Detailed { ordinal }));
    }
    assert_eq!(
        first.admit(),
        Some(Admission::SuppressionNotice { ordinal: 5 })
    );
    assert_eq!(first.admit(), None);
    assert_eq!(second.admit(), Some(Admission::Detailed { ordinal: 1 }));
}

#[test]
fn owned_tickets_retain_complete_context() {
    let diagnostics = VmDiagnostics::new();
    let identity = VmDiagnosticId::new(3, 7);
    let read = MmioAccess::new(
        GuestPhysicalAddress::new(0x1234),
        AccessWidth::Word,
        MmioOperation::Read,
    );
    let report = crate::require_some(diagnostics.admit_unhandled_mmio(identity, 2, read));
    assert_eq!(
        std::format!("{report}"),
        "HypeR: unhandled guest MMIO: VM 3:7, vCPU 2, GPA 0x1234, width Word, read, occurrence 1"
    );

    let write = MmioAccess::new(
        GuestPhysicalAddress::new(0x5678),
        AccessWidth::DoubleWord,
        MmioOperation::Write(0xabcd),
    );
    let report = crate::require_some(diagnostics.admit_unhandled_mmio(identity, 2, write));
    assert!(std::format!("{report}").contains("write 0xabcd, occurrence 2"));
}
