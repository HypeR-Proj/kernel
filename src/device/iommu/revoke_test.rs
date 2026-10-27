// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Disposable QEMU fixture: revoke one live I/O VM page without releasing RAM.
//! No syscall, guest protocol, descriptor modification or production entrypoint.

use super::address_space::AddressSpace;
use core::sync::atomic::{AtomicU64, Ordering};
use hyper::mm::FallibleArc;
use {crate::mm::memory, crate::mm::page_block::PageBlock, crate::sync::Mutex, crate::task};

struct Fixture {
    space: FallibleArc<AddressSpace>,
    control: PageBlock,
}

// Test owners deliberately survive until QEMU exits, including error paths.
static FIXTURE: Mutex<Option<Fixture>> = Mutex::new(None);

pub(crate) fn start(space: FallibleArc<AddressSpace>) {
    let control = PageBlock::allocate(0).unwrap_or_else(|error| fail(error));
    let address =
        memory::linear_address(control.physical().get()).unwrap_or_else(|| fail("control address"));
    // SAFETY: This new, aligned page is exclusively owned and not yet published
    // to either the worker or the external debugger. Only its first word is used.
    unsafe { (address as *mut AtomicU64).write(AtomicU64::new(0)) };
    let mut fixture = FIXTURE.lock().unwrap_or_else(|error| fail(error));
    if fixture.is_some() {
        fail("multiple I/O VM fixtures");
    }
    *fixture = Some(Fixture { space, control });
    drop(fixture);
    let thread = task::scheduler::kthread_create("kdma-revoke-test", worker, 0)
        .unwrap_or_else(|error| fail(error));
    if task::scheduler::thread_ready(thread) != Ok(true) {
        fail("test worker start");
    }
}

extern "C" fn worker(_: usize) {
    let (space, physical) = {
        let fixture = FIXTURE.lock().unwrap_or_else(|error| fail(error));
        let fixture = fixture.as_ref().unwrap_or_else(|| fail("missing fixture"));
        (fixture.space.clone(), fixture.control.physical().get())
    };
    let address = memory::linear_address(physical).unwrap_or_else(|| fail("control address"));
    // SAFETY: FIXTURE permanently owns this page. The QEMU harness writes one
    // aligned 64-bit request while all emulated CPUs are stopped, then resumes.
    // No Native or guest mapping exposes the control page.
    let request = unsafe { &*(address as *const AtomicU64) };
    crate::pr_info!("DMA-REVOKE: ready control={physical:#x} sid=16");
    let iova = loop {
        let iova = request.load(Ordering::Acquire);
        if iova != 0 {
            break iova;
        }
        task::sleep_ms(10).unwrap_or_else(|error| fail(error));
    };
    let _retained = space
        .revoke_test_page(iova)
        .unwrap_or_else(|error| fail(error));
    crate::pr_info!("DMA-REVOKE: unmapped iova={iova:#x}; CMD_SYNC complete; backing retained");
    // No restore: an ensuing real fault permanently quarantines its SID.
    // Returning/reusing this page or resuming the device would hide that policy.
    loop {
        task::sleep_s(3600).unwrap_or_else(|error| fail(error));
    }
}

fn fail(error: impl core::fmt::Debug) -> ! {
    crate::diagnostics::crash::fatal(format_args!("DMA-REVOKE: fixture failed: {error:?}"));
}
