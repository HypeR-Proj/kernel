// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::mount_admission::Admission;
use loom::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

#[test]
fn lookup_and_unmount_cannot_both_admit() {
    loom::model(|| {
        let admission = Arc::new(Admission::new());
        let retired = Arc::new(AtomicBool::new(false));
        let lookup = {
            let admission = admission.clone();
            let retired = retired.clone();
            thread::spawn(move || {
                if admission.acquire() {
                    assert!(!retired.load(Ordering::Acquire));
                    thread::yield_now();
                    assert!(!retired.load(Ordering::Acquire));
                    admission.release();
                }
            })
        };
        if admission.freeze() {
            retired.store(true, Ordering::Release);
            assert!(!admission.acquire());
        }
        crate::require_ok(lookup.join());
    });
}

#[test]
fn clone_of_existing_location_prevents_freeze() {
    loom::model(|| {
        let admission = Arc::new(Admission::new());
        assert!(admission.acquire());
        let clone = {
            let admission = admission.clone();
            thread::spawn(move || {
                admission.retain();
                thread::yield_now();
                admission.release();
            })
        };
        assert!(!admission.freeze());
        crate::require_ok(clone.join());
        admission.release();
        assert!(admission.freeze());
        assert!(!admission.acquire());
        admission.thaw();
        assert!(admission.acquire());
        admission.release();
    });
}

#[test]
fn failed_freeze_preserves_live_clones_and_remains_closed() {
    loom::model(|| {
        let admission = Arc::new(Admission::new());
        assert!(admission.acquire());
        let clone = {
            let admission = admission.clone();
            thread::spawn(move || {
                admission.retain();
                thread::yield_now();
                admission.release();
            })
        };
        assert!(admission.freeze_failed());
        assert!(!admission.freeze_failed());
        assert!(!admission.acquire());
        crate::require_ok(clone.join());
        admission.release();
        assert!(!admission.acquire());
        admission.thaw();
        assert!(admission.freeze());
    });
}

#[test]
fn failed_freeze_rollback_preserves_racing_reference_count() {
    loom::model(|| {
        let admission = Arc::new(Admission::new());
        assert!(admission.acquire());
        assert!(admission.freeze_failed());
        let release = {
            let admission = admission.clone();
            thread::spawn(move || admission.release())
        };
        admission.thaw();
        assert!(admission.acquire());
        crate::require_ok(release.join());
        assert!(!admission.freeze());
        admission.release();
        assert!(admission.freeze());
    });
}
