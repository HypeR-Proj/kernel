// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Call-site syntax is compiled both with and without instrumentation.

#[cfg(feature = "kernel-trace")]
fn adapter(
    ticket: hyper_trace::Ticket,
    site: &'static hyper_trace::Site,
    fields: &mut dyn FnMut() -> [u64; 4],
) {
    hyper_trace::record_at(ticket, 0, site, || Some(123), fields);
}

#[test]
fn macro_erases_or_lazily_evaluates_arguments_and_describes_itself() {
    use std::cell::Cell;
    let count = Cell::new(0u64);
    hyper_trace::tracepoint!(
        Test,
        "disabled",
        value = {
            count.set(count.get() + 1);
            count.get()
        }
    );
    assert_eq!(count.get(), 0);
    #[cfg(not(feature = "kernel-trace"))]
    hyper_trace::tracepoint!(
        Test,
        "erased",
        value = missing_in_a_non_instrumented_build()
    );
    #[cfg(feature = "kernel-trace")]
    {
        use {
            hyper_trace as trace, hyper_trace::Category, hyper_trace::Error, hyper_trace::Filter,
        };
        assert_eq!(trace::start(Filter::ALL), Err(Error::Unavailable));
        crate::require_ok(trace::install(adapter));
        assert_eq!(trace::install(adapter), Err(Error::AlreadyInitialized));
        let session = crate::require_ok(trace::start(Filter::only(Category::Test)));
        hyper_trace::tracepoint!(
            Io,
            "filtered",
            value = {
                count.set(9);
                9
            }
        );
        assert_eq!(count.get(), 0);
        hyper_trace::tracepoint!(
            Test,
            "four",
            a = 1,
            b = 2,
            c = 3,
            d = {
                count.set(1);
                4
            }
        );
        hyper_trace::tracepoint!(Test, "no_fields");
        crate::require_ok(trace::stop(session));
        let mut out = [None; 8];
        assert_eq!(
            crate::require_ok(trace::read(session, 0, 0, &mut out)).count,
            2
        );
        assert_eq!(count.get(), 1);
        let first = crate::require_some(out[0]);
        assert_eq!(first.site.name, "four");
        assert_eq!(first.site.fields, &["a", "b", "c", "d"]);
        assert_eq!(first.values, [1, 2, 3, 4]);
        assert_eq!(first.site.file, file!());
        assert!(first.site.line > 0);
        let second = crate::require_some(out[1]);
        assert!(second.site.fields.is_empty());
        assert_eq!(second.values, [0; 4]);
        crate::require_ok(trace::clear(session));
    }
}
