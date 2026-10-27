// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

/// Record up to four named u64 scalars with one call-site declaration.
///
/// `tracepoint!(Scheduler, "thread_ready", thread = id, cpu = target)`
///
/// `tracepoint!(ticket = admission, Ipc, "rpc_end", request = id)` retains an
/// existing admission snapshot across a generation check; it cannot enable a
/// capture or publish into a replacement session.
///
/// Fields must be nonblocking, side-effect-free scalar expressions. They are
/// evaluated only after admission, with local IRQs masked and the ring owned.
/// There is no timestamp, argument evaluation or site metadata in builds without
/// `kernel-trace`. Categories and names are independent of Perfetto's wire format.
#[cfg(feature = "kernel-trace")]
#[macro_export]
macro_rules! tracepoint {
    (ticket = $ticket:expr, $category:ident, $name:literal $(, $field:ident = $value:expr)* $(,)?) => {{
        static SITE: $crate::Site = $crate::Site::new(
            $crate::Category::$category, $name, &[$(stringify!($field)),*], file!(), line!(),
        );
        $crate::emit($ticket, &SITE, || $crate::values([$($value),*]));
    }};
    ($category:ident, $name:literal $(, $field:ident = $value:expr)* $(,)?) => {{
        if let Some(ticket) = $crate::enabled($crate::Category::$category) {
            static SITE: $crate::Site = $crate::Site::new(
                $crate::Category::$category, $name, &[$(stringify!($field)),*], file!(), line!(),
            );
            $crate::emit(ticket, &SITE, || $crate::values([$($value),*]));
        }
    }};
}

#[cfg(not(feature = "kernel-trace"))]
/// One-line kernel event instrumentation, erased when `kernel-trace` is off.
/// Accepts a category, literal event name and up to four named scalar fields;
/// the disabled expansion evaluates none of them. See the crate-level documentation.
#[macro_export]
macro_rules! tracepoint {
    (ticket = $ticket:expr, $category:ident, $name:literal $(, $field:ident = $value:expr)* $(,)?) => {{}};
    ($category:ident, $name:literal $(, $field:ident = $value:expr)* $(,)?) => {{}};
}
