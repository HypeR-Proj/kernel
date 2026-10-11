// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::protection_state::ProtectionState;
use loom::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

struct File {
    content_gate: Mutex<()>,
    protections: ProtectionState,
    version: AtomicUsize,
}

#[test]
fn protection_acquisition_excludes_existing_and_future_mutations() {
    loom::model(|| {
        let file = Arc::new(File {
            content_gate: Mutex::new(()),
            protections: ProtectionState::new(),
            version: AtomicUsize::new(0),
        });
        let writer = {
            let file = file.clone();
            thread::spawn(move || {
                let _gate = crate::require_ok(file.content_gate.lock());
                if file.protections.writable() {
                    file.version.store(1, Ordering::Relaxed);
                    thread::yield_now();
                    file.version.store(2, Ordering::Relaxed);
                }
            })
        };
        let observed = {
            let _gate = crate::require_ok(file.content_gate.lock());
            assert!(file.protections.acquire());
            file.version.load(Ordering::Relaxed)
        };
        assert!(observed == 0 || observed == 2);
        thread::yield_now();
        assert_eq!(file.version.load(Ordering::Relaxed), observed);
        file.protections.release();
        crate::require_ok(writer.join());
    });
}

#[test]
fn process_exit_does_not_release_another_mapping_owner() {
    loom::model(|| {
        let state = Arc::new(ProtectionState::new());
        // Serialized acquisition of independent protection owners.
        assert!(state.acquire());
        assert!(state.acquire());
        let exiting_process = {
            let state = state.clone();
            thread::spawn(move || state.release())
        };
        assert!(!state.writable());
        thread::yield_now();
        assert!(!state.writable());
        crate::require_ok(exiting_process.join());
        assert!(!state.writable());
        state.release();
        assert!(state.writable());
    });
}
