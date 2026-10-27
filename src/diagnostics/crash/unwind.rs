// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded frame-chain validation and symbol lookup for frozen CPUs.
//!
//! This module reads only a crash snapshot and a pinned stopped-CPU stack. It
//! does not coordinate CPUs or publish state. Every unsafe frame read remains
//! behind architecture validation, and traversal is bounded and allocation-free.

use crate::hal::exception::CrashContext;

const MAX_BACKTRACE_DEPTH: usize = 32;

pub(super) fn dump_backtrace(
    cpu: usize,
    context: &CrashContext,
    task: Option<crate::task::scheduler::CrashTaskSnapshot>,
) {
    crate::diagnostics::log::emergency(format_args!("CPU {cpu} Call trace:"));
    emit_trace_entry(0, context.program_counter, context.program_counter);
    if !context.general_is_valid(CrashContext::FRAME_POINTER_REGISTER) {
        crate::diagnostics::log::emergency(format_args!("  frame pointer unavailable"));
        return;
    }
    let Some((bottom, top)) = stack_bounds(cpu, context.stack_pointer, task) else {
        crate::diagnostics::log::emergency(format_args!(
            "  stack bounds unavailable; unwind stopped"
        ));
        return;
    };
    walk_frame_chain(
        context.general[CrashContext::FRAME_POINTER_REGISTER] as usize,
        bottom,
        top,
    );
}

fn stack_bounds(
    cpu: usize,
    stack_pointer: u64,
    task: Option<crate::task::scheduler::CrashTaskSnapshot>,
) -> Option<(usize, usize)> {
    task.and_then(|task| task.stack)
        .filter(|(bottom, top)| *bottom <= stack_pointer as usize && stack_pointer as usize <= *top)
        .or_else(|| crate::hal::exception::bootstrap_stack_bounds(stack_pointer))
        .or_else(|| crate::mm::stack::exception_stack_bounds(cpu, stack_pointer as usize))
}

fn walk_frame_chain(mut frame: usize, bottom: usize, top: usize) {
    for depth in 1..MAX_BACKTRACE_DEPTH {
        // SAFETY: stack_bounds returns a pinned live kernel stack owned by the
        // stopped CPU. Crash coordination has stopped concurrent execution on
        // that stack, and previous_stack_frame validates each record extent
        // and alignment before reading its initialized frame words.
        let record = match unsafe { CrashContext::previous_stack_frame(frame, bottom, top) } {
            Ok(record) => record,
            Err(_) => {
                crate::diagnostics::log::emergency(format_args!(
                    "  invalid frame pointer {frame:#x}; unwind stopped"
                ));
                return;
            }
        };
        let Some((previous, link)) = record else {
            return;
        };
        emit_trace_entry(depth, link as u64, (link as u64).saturating_sub(4));
        if previous <= frame {
            return;
        }
        frame = previous;
    }
    crate::diagnostics::log::emergency(format_args!(
        "  backtrace truncated at {MAX_BACKTRACE_DEPTH} entries"
    ));
}

fn emit_trace_entry(depth: usize, address: u64, _lookup_address: u64) {
    #[cfg(feature = "runtime-symbols")]
    if let Some(symbol) = usize::try_from(_lookup_address).ok().and_then(|address| {
        crate::diagnostics::debug::kallsyms::lookup(address)
            .ok()
            .flatten()
    }) {
        crate::diagnostics::log::emergency(format_args!(
            "  #{depth:02} [<{address:#018x}>] {symbol}"
        ));
        return;
    }
    crate::diagnostics::log::emergency(format_args!("  #{depth:02} [<{address:#018x}>]"));
}

pub(super) fn emit_symbolized(label: &str, address: u64, _lookup_address: u64) {
    #[cfg(feature = "runtime-symbols")]
    if let Some(symbol) = usize::try_from(_lookup_address).ok().and_then(|address| {
        crate::diagnostics::debug::kallsyms::lookup(address)
            .ok()
            .flatten()
    }) {
        crate::diagnostics::log::emergency(format_args!("{label}: {address:#018x} <{symbol}>"));
        return;
    }
    crate::diagnostics::log::emergency(format_args!("{label}: {address:#018x}"));
}
