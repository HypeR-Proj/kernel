// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::sync::InterruptMaskGuard;
use {hyper_trace::Error, hyper_trace::Site, hyper_trace::Ticket};

/// Installs the machine seam once. Capture remains disabled until explicitly
/// started; one ordinary expiry callout is allocated but not armed.
pub fn initialize() -> Result<(), Error> {
    super::control::initialize()?;
    hyper_trace::install(record)
}

/// IRQ masking keeps CPU selection, timestamp and append on the same processor.
/// The try-only recorder also tolerates recursion without spinning on its owner.
fn record(ticket: Ticket, site: &'static Site, fields: &mut dyn FnMut() -> [u64; 4]) {
    // SAFETY: This bounded nonblocking scope never yields or migrates. The guard
    // restores the caller's prior mask on this CPU, including nested IRQ callers.
    let _mask = unsafe { InterruptMaskGuard::<crate::hal::irq::LocalMask>::acquire() };
    if let Some(cpu) = crate::cpu::current_index() {
        // Internal self-tests use the foundation API without capture policy.
        if !super::control::selected(cpu.get()) {
            return;
        }
        hyper_trace::record_at(
            ticket,
            cpu.get(),
            site,
            || crate::time::monotonic_nanoseconds().ok(),
            fields,
        );
    }
}
