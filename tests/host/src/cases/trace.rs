// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercise the production recorder, including blocked writers and session reuse.
use hyper::trace::{Category, Error, Filter, Recorder, Site};
use std::sync::{Arc, Barrier};

static SITE: Site = Site::new(Category::Scheduler, "ready", &["thread"], file!(), line!());
static IPC: Site = Site::new(Category::Ipc, "send", &[], file!(), line!());
fn emit<const N: usize, const C: usize>(r: &Recorder<N, C>, cpu: usize, value: u64) {
    let ticket = crate::require_some(r.enabled(SITE.category));
    r.record(ticket, cpu, &SITE, || Some(value), || [value, 0, 0, 0]);
}

#[test]
fn runtime_filters_pause_without_discarding_the_capture() {
    let r = Recorder::<1, 4>::new();
    assert_eq!(r.start(Filter::NONE), Err(Error::InvalidFilter));
    let session = crate::require_ok(r.start(Filter::ALL));
    emit(&r, 0, 17);
    let ticket = crate::require_some(r.enabled(Category::Scheduler));
    crate::require_ok(r.set_filter(session, Filter::NONE));
    assert!(r.enabled(Category::Scheduler).is_none());
    r.record(
        ticket,
        0,
        &SITE,
        || panic!("paused clock"),
        || panic!("paused fields"),
    );
    crate::require_ok(
        r.set_filter(
            session,
            Filter::only(Category::Scheduler)
                .with(Category::Ipc)
                .without(Category::Ipc),
        ),
    );
    assert!(r.enabled(Category::Scheduler).is_some());
    assert!(r.enabled(Category::Ipc).is_none());
    emit(&r, 0, 18);
    crate::require_ok(r.stop(session));
    assert_eq!(r.set_filter(session, Filter::ALL), Err(Error::NotRecording));
    let mut out = [None; 4];
    assert_eq!(crate::require_ok(r.read(session, 0, 0, &mut out)).count, 2);
    assert_eq!(crate::require_some(out[0]).values[0], 17);
}

#[test]
fn filtering_laziness_and_stale_tickets() {
    let r = Recorder::<1, 4>::new();
    assert!(r.enabled(Category::Scheduler).is_none());
    let first = crate::require_ok(r.start(Filter::only(Category::Scheduler)));
    assert!(r.enabled(Category::Ipc).is_none());
    let old = crate::require_some(r.enabled(Category::Scheduler));
    assert_eq!(old.generation(), first.generation());
    r.record(
        old,
        0,
        &IPC,
        || panic!("filtered clock"),
        || panic!("filtered fields"),
    );
    crate::require_ok(r.stop(first));
    let second = crate::require_ok(r.start(Filter::ALL));
    assert_ne!(first, second);
    r.record(
        old,
        0,
        &SITE,
        || panic!("stale clock"),
        || panic!("stale fields"),
    );
    let mut out = [None; 4];
    assert_eq!(crate::require_ok(r.read(second, 0, 0, &mut out)).count, 0);
    assert!(matches!(
        r.read(first, 0, 0, &mut out),
        Err(Error::InvalidSession)
    ));
    assert_eq!(r.stop(first), Err(Error::InvalidSession));
    assert_eq!(r.start(Filter::ALL), Err(Error::Busy));
    crate::require_ok(r.stop(second));
}

#[test]
fn ring_wrap_batch_cursors_and_lazy_reset() {
    let r = Recorder::<2, 4>::new();
    let session = crate::require_ok(r.start(Filter::ALL));
    for value in 0..11 {
        emit(&r, 0, value);
    }
    emit(&r, 1, 90);
    crate::require_ok(r.stop(session));
    let mut out = [None; 2];
    let batch = crate::require_ok(r.read(session, 0, 0, &mut out));
    assert_eq!((batch.count, batch.next, batch.overwritten), (2, 9, 7));
    for (entry, expected) in out.iter().zip(7..9) {
        let entry = crate::require_some(*entry);
        assert_eq!(entry.sequence, expected);
        assert_eq!(entry.timestamp_ns, expected);
        assert_eq!(entry.values[0], expected);
        assert_eq!(entry.site.fields, &["thread"]);
    }
    assert!(!batch.incomplete);
    assert_eq!(
        crate::require_ok(r.read(session, 0, batch.next, &mut out)).next,
        11
    );
    assert_eq!(
        crate::require_ok(r.read(session, 0, 100, &mut out)).next,
        100
    );
    assert_eq!(crate::require_ok(r.read(session, 1, 0, &mut out)).count, 1);
    assert!(matches!(
        r.read(session, 2, 0, &mut out),
        Err(Error::InvalidCpu)
    ));
    let second = crate::require_ok(r.start(Filter::ALL));
    assert_eq!(crate::require_ok(r.read(second, 0, 0, &mut out)).count, 0);
    emit(&r, 0, 42);
    let batch = crate::require_ok(r.read(second, 0, 0, &mut out));
    assert_eq!((batch.count, batch.next, batch.overwritten), (1, 1, 0));
    assert_eq!(crate::require_some(out[0]).values[0], 42);
}

#[test]
fn recursive_writer_drops_without_evaluating_payload() {
    let r = Recorder::<1, 4>::new();
    let session = crate::require_ok(r.start(Filter::ALL));
    let ticket = crate::require_some(r.enabled(Category::Scheduler));
    r.record(
        ticket,
        0,
        &SITE,
        || Some(1),
        || {
            r.record(
                ticket,
                0,
                &SITE,
                || panic!("recursive clock"),
                || panic!("recursive fields"),
            );
            [7; 4]
        },
    );
    crate::require_ok(r.stop(session));
    let mut out = [None; 4];
    let batch = crate::require_ok(r.read(session, 0, 0, &mut out));
    assert_eq!(batch.count, 1);
    assert!(batch.incomplete);
    let next = crate::require_ok(r.start(Filter::ALL));
    assert!(!crate::require_ok(r.read(next, 0, 0, &mut out)).incomplete);
}

#[test]
fn stop_joins_an_admitted_writer_and_cannot_recycle_early() {
    let r = Arc::new(Recorder::<2, 8>::new());
    let session = crate::require_ok(r.start(Filter::ALL));
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let writer = {
        let (r, entered, release) = (r.clone(), entered.clone(), release.clone());
        std::thread::spawn(move || {
            let ticket = crate::require_some(r.enabled(Category::Scheduler));
            r.record(
                ticket,
                1,
                &SITE,
                || Some(8),
                || {
                    // Test-only suspension models an interrupted in-flight producer.
                    entered.wait();
                    release.wait();
                    [88; 4]
                },
            );
        })
    };
    entered.wait();
    let stop = r.stop(session);
    let restart = r.start(Filter::ALL);
    let disabled = r.enabled(Category::Scheduler).is_none();
    let mut out = [None; 4];
    let busy_read = matches!(r.read(session, 1, 0, &mut out), Err(Error::Busy));
    release.wait();
    crate::require_ok(writer.join());
    assert_eq!(stop, Err(Error::Busy));
    assert_eq!(restart, Err(Error::Busy));
    assert!(disabled && busy_read);
    crate::require_ok(r.stop(session));
    assert_eq!(crate::require_ok(r.read(session, 1, 0, &mut out)).count, 1);
    assert_eq!(crate::require_some(out[0]).values[0], 88);
    let next = crate::require_ok(r.start(Filter::ALL));
    assert_eq!(crate::require_ok(r.read(next, 1, 0, &mut out)).count, 0);
}

#[test]
fn readers_are_bounded_and_missing_clock_is_visible() {
    let r = Recorder::<1, 64>::new();
    let session = crate::require_ok(r.start(Filter::ALL));
    let ticket = crate::require_some(r.enabled(Category::Scheduler));
    r.record(ticket, 0, &SITE, || None, || panic!("clock unavailable"));
    for n in 0..60 {
        emit(&r, 0, n);
    }
    let mut out = [None; 64];
    let batch = crate::require_ok(r.read(session, 0, 0, &mut out));
    assert_eq!(batch.count, 32);
    assert!(batch.incomplete);
}

#[test]
fn independent_cpu_writers_and_reads_preserve_whole_records() {
    let r = Arc::new(Recorder::<2, 64>::new());
    let session = crate::require_ok(r.start(Filter::ALL));
    std::thread::scope(|scope| {
        for cpu in 0..2 {
            let r = &r;
            scope.spawn(move || {
                for n in 0..5000 {
                    emit(r, cpu, n);
                }
            });
        }
        let mut out = [None; 8];
        for _ in 0..200 {
            for cpu in 0..2 {
                if let Ok(batch) = r.read(session, cpu, 0, &mut out) {
                    for entry in out.iter().take(batch.count).flatten() {
                        assert_eq!(entry.timestamp_ns, entry.values[0]);
                        assert_eq!(entry.site.name, "ready");
                    }
                }
            }
        }
    });
    crate::require_ok(r.stop(session));
}

#[test]
fn stale_contended_writer_cannot_mark_the_replacement_capture_incomplete() {
    let r = Arc::new(Recorder::<1, 4>::new());
    let first = crate::require_ok(r.start(Filter::ALL));
    let stale = crate::require_some(r.enabled(Category::Scheduler));
    crate::require_ok(r.stop(first));
    let second = crate::require_ok(r.start(Filter::ALL));
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let writer = {
        let (r, entered, release) = (r.clone(), entered.clone(), release.clone());
        std::thread::spawn(move || {
            let ticket = crate::require_some(r.enabled(Category::Scheduler));
            r.record(
                ticket,
                0,
                &SITE,
                || Some(1),
                || {
                    entered.wait();
                    release.wait();
                    [2; 4]
                },
            );
        })
    };
    entered.wait();
    r.record(
        stale,
        0,
        &SITE,
        || panic!("stale clock"),
        || panic!("stale payload"),
    );
    release.wait();
    crate::require_ok(writer.join());
    crate::require_ok(r.stop(second));
    let mut out = [None; 1];
    let batch = crate::require_ok(r.read(second, 0, 0, &mut out));
    assert_eq!(batch.count, 1);
    assert!(!batch.incomplete);
    assert_eq!(core::mem::size_of::<Option<hyper::trace::Record>>(), 56);
}

#[test]
fn runtime_capacity_wraps_and_can_grow_only_between_captures() {
    let r = Recorder::<1, 8>::new();
    assert_eq!(
        r.start_with_capacity(Filter::ALL, 0),
        Err(Error::InvalidFilter)
    );
    assert_eq!(
        r.start_with_capacity(Filter::ALL, 9),
        Err(Error::InvalidFilter)
    );
    let first = crate::require_ok(r.start_with_capacity(Filter::ALL, 2));
    for value in 0..5 {
        emit(&r, 0, value);
    }
    assert_eq!(r.start_with_capacity(Filter::ALL, 8), Err(Error::Busy));
    let mut out = [None; 8];
    let batch = crate::require_ok(r.read(first, 0, 0, &mut out));
    assert_eq!((batch.count, batch.next, batch.overwritten), (2, 5, 3));
    assert_eq!(crate::require_some(out[0]).sequence, 3);
    crate::require_ok(r.stop(first));
    let second = crate::require_ok(r.start_with_capacity(Filter::ALL, 8));
    assert_eq!(crate::require_ok(r.read(second, 0, 0, &mut out)).count, 0);
    for value in 0..7 {
        emit(&r, 0, value);
    }
    let batch = crate::require_ok(r.read(second, 0, 0, &mut out));
    assert_eq!((batch.count, batch.next, batch.overwritten), (7, 7, 0));
    crate::require_ok(r.stop(second));
}
