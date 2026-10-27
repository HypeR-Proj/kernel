// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free rendering of immutable crash snapshots.
//!
//! Reporting consumes state frozen by crash coordination and may consult
//! scheduler and symbol metadata with their crash-safe, non-blocking APIs. It
//! does not publish crash state, send stop requests, or dereference stack frames
//! directly; bounded frame validation belongs to `unwind`.

use core::fmt;

use crate::hal::exception::CrashContext;

/// Frozen result of the crash owner's bounded remote-stop attempt.
#[derive(Clone, Copy)]
pub(super) struct StopSummary {
    pub(super) expected: usize,
    pub(super) stopped: usize,
    pub(super) sent: bool,
}

pub(super) fn emit_banner(cpu: usize, reason: &str, reason_was_truncated: bool, stop: StopSummary) {
    crate::diagnostics::log::emergency(format_args!(
        "============================================================"
    ));
    crate::diagnostics::log::emergency(format_args!("HypeR KERNEL PANIC - NOT SYNCING"));
    crate::diagnostics::log::emergency(format_args!(
        "============================================================"
    ));
    crate::diagnostics::log::emergency(format_args!("BUG: fatal kernel failure on CPU {cpu}"));
    crate::diagnostics::log::emergency(format_args!("{reason}"));
    if reason_was_truncated {
        crate::diagnostics::log::emergency(format_args!("[crash reason truncated]"));
    }
    if stop.expected == 0 {
        crate::diagnostics::log::emergency(format_args!("SMP: no other online CPUs"));
    } else if !stop.sent {
        crate::diagnostics::log::emergency(format_args!(
            "SMP: crash-stop IPI unavailable; {} CPU(s) may still be running",
            stop.expected
        ));
    } else {
        crate::diagnostics::log::emergency(format_args!(
            "SMP: stopped {}/{} other online CPU(s)",
            stop.stopped, stop.expected
        ));
    }
}

pub(super) fn dump_cpu_states(owner: usize) {
    let online = crate::cpu::online_cpu_count().max(owner.saturating_add(1));
    for (cpu, slot) in super::state::contexts()
        .iter()
        .enumerate()
        .take(online.min(super::state::MAX_CPUS))
    {
        let Some(context) = slot.read() else {
            crate::diagnostics::log::emergency(format_args!(
                "CPU {cpu}: ONLINE, failed to stop or state unavailable"
            ));
            continue;
        };
        let role = if cpu == owner { "crashing" } else { "stopped" };
        let task = crate::task::scheduler::crash_snapshot(cpu);
        dump_cpu_header(cpu, role, context, task);
        dump_registers(&context);
        super::unwind::dump_backtrace(cpu, &context, task);
    }
}

fn dump_cpu_header(
    cpu: usize,
    role: &str,
    context: CrashContext,
    task: Option<crate::task::scheduler::CrashTaskSnapshot>,
) {
    context.describe_cpu_header(cpu, role, crate::diagnostics::log::emergency);
    match task {
        Some(task) => {
            crate::diagnostics::log::emergency(format_args!(
                "CPU {cpu}: task {} {:?} {:?}",
                task.id.get(),
                task.state,
                task.execution
            ));
            if let Some(stack) = task.stack_statistics {
                crate::diagnostics::log::emergency(format_args!(
                    "CPU {cpu}: task stack {}/{} bytes used, guard {:#x}, canary {}",
                    stack.used,
                    stack.size,
                    stack.guard_page,
                    if stack.canary_intact {
                        "intact"
                    } else {
                        "CORRUPTED"
                    }
                ));
            }
        }
        None => crate::diagnostics::log::emergency(format_args!(
            "CPU {cpu}: current task unavailable (scheduler lock busy or not initialized)"
        )),
    }
}

pub(super) fn dump_registers(context: &CrashContext) {
    dump_general_registers(context, CrashContext::GENERAL_REGISTER_COUNT);
    super::unwind::emit_symbolized("PC", context.program_counter, context.program_counter);
    crate::diagnostics::log::emergency(format_args!(
        "SP: {:#018x}  STATUS: {:#018x}",
        context.stack_pointer, context.processor_state
    ));
    context.describe_architecture_registers(crate::diagnostics::log::emergency);
}

fn dump_general_registers(context: &CrashContext, register_count: usize) {
    for base in (0..register_count).step_by(4) {
        let remaining = register_count - base;
        if remaining < 4 {
            match remaining {
                1 => crate::diagnostics::log::emergency(format_args!(
                    "x{base:02}: {}",
                    RegisterValue::new(context, base)
                )),
                2 => crate::diagnostics::log::emergency(format_args!(
                    "x{base:02}: {}  x{:02}: {}",
                    RegisterValue::new(context, base),
                    base + 1,
                    RegisterValue::new(context, base + 1)
                )),
                3 => crate::diagnostics::log::emergency(format_args!(
                    "x{base:02}: {}  x{:02}: {}  x{:02}: {}",
                    RegisterValue::new(context, base),
                    base + 1,
                    RegisterValue::new(context, base + 1),
                    base + 2,
                    RegisterValue::new(context, base + 2)
                )),
                _ => {}
            }
            break;
        }
        crate::diagnostics::log::emergency(format_args!(
            "x{base:02}: {}  x{:02}: {}  x{:02}: {}  x{:02}: {}",
            RegisterValue::new(context, base),
            base + 1,
            RegisterValue::new(context, base + 1),
            base + 2,
            RegisterValue::new(context, base + 2),
            base + 3,
            RegisterValue::new(context, base + 3)
        ));
    }
}

struct RegisterValue {
    valid: bool,
    value: u64,
}

impl RegisterValue {
    fn new(context: &CrashContext, register: usize) -> Self {
        Self {
            valid: context.general_is_valid(register),
            value: context.general.get(register).copied().unwrap_or(0),
        }
    }
}

impl fmt::Display for RegisterValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.valid {
            write!(formatter, "{:#018x}", self.value)
        } else {
            formatter.write_str("??????????????????")
        }
    }
}
