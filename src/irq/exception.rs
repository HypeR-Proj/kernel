// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fatal exception policy above architecture vector decoding.

use core::fmt;

use hyper::hal::interrupt::InterruptId;

pub fn fatal_interrupt(reason: &str, interrupt: Option<InterruptId>) -> ! {
    match interrupt {
        Some(interrupt) => enter_fatal(format_args!(
            "HypeR: fatal IRQ dispatch: {reason}; INTID {}",
            interrupt.get()
        )),
        None => enter_fatal(format_args!("HypeR: fatal IRQ dispatch: {reason}")),
    }
}

pub fn fatal_timer(error: impl fmt::Debug) -> ! {
    enter_fatal(format_args!("HypeR: fatal timer interrupt: {error:?}"))
}

pub fn fatal_interrupt_state(error: super::interrupt::Error) -> ! {
    enter_fatal(format_args!(
        "HypeR: fatal interrupt-controller state: {error:?}"
    ))
}

fn enter_fatal(arguments: fmt::Arguments<'_>) -> ! {
    crate::diagnostics::crash::fatal(arguments)
}
