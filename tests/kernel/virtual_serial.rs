// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Registration, hostile cursor, runtime-loss, and pinned-page retirement proof.

use crate::kernel::accounting::{ResourceDomain, ResourceKind, ResourceLimits};
use crate::kernel::mm::user_space::VmoObject;
use crate::kernel::object::{KernelObject, ObjectRetirement};
use crate::kernel::vm::virtual_serial::VirtualSerial;
use core::sync::atomic::{AtomicU64, Ordering};
use hyper::abi::native::{
    HYPER_NATIVE_VIRTUAL_SERIAL_OUTPUT_BYTES as BYTES,
    HYPER_NATIVE_VIRTUAL_SERIAL_OUTPUT_CAPACITY as CAPACITY,
};

pub(super) fn run() -> Result<(), &'static str> {
    let domain = ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| "domain")?;
    let serial = VirtualSerial::try_new(&domain).map_err(|_| "port")?;
    let wrong = VmoObject::try_new_writable(4096, &domain).map_err(|_| "wrong size VMO")?;
    if serial.register_output(&wrong, &domain).is_ok() {
        return Err("wrong registration size accepted");
    }
    drop(wrong);
    let buffer = VmoObject::try_new_writable(BYTES, &domain).map_err(|_| "buffer")?;
    serial
        .register_output(&buffer, &domain)
        .map_err(|_| "register")?;
    if serial.register_output(&buffer, &domain).is_ok() {
        return Err("duplicate registration");
    }
    if !serial.claim_assignment() || serial.claim_assignment() {
        return Err("multiple producers admitted");
    }
    let storage = buffer.writable().ok_or("writable storage")?;
    let producer_address = crate::kernel::mm::memory::linear_address(
        storage
            .resident_physical_page(0)
            .map_err(|_| "producer page")?
            .get(),
    )
    .ok_or("producer alias")?;
    // SAFETY: registration pins initialized, aligned storage through serial
    // lifetime. The test observes the same atomic header as a read-only client.
    let producer = unsafe { &*core::ptr::with_exposed_provenance::<AtomicU64>(producer_address) };
    if storage.try_mapping_write_lease().is_ok() || storage.write(0, &[1]).is_ok() {
        return Err("registered output admitted another writer");
    }
    drop(buffer);
    if domain.usage().committed(ResourceKind::PinnedPages) != BYTES / 4096 {
        return Err("registration did not retain pinned charge");
    }
    for _ in 0..CAPACITY {
        serial.publish_guest_output(b'x');
    }
    if serial.acknowledge_output(u64::MAX).is_ok() {
        return Err("future cursor accepted");
    }
    serial.publish_guest_output(b'y');
    if producer.load(Ordering::Acquire) != CAPACITY {
        return Err("untrusted cursor released full buffer");
    }
    serial
        .acknowledge_output(1)
        .map_err(|_| "valid acknowledgement")?;
    serial.publish_guest_output(b'z');
    if producer.load(Ordering::Acquire) != CAPACITY + 1 {
        return Err("consumed slot not reused");
    }
    if serial.acknowledge_output(0).is_ok() {
        return Err("regressing cursor accepted");
    }
    serial.publish_guest_output(b'w');
    if producer.load(Ordering::Acquire) != CAPACITY + 1 {
        return Err("regressed cursor accepted");
    }
    let readable = hyper::abi::native::HYPER_NATIVE_SIGNAL_VIRTUAL_SERIAL_READABLE;
    if serial.observed_signals() & readable == 0 {
        return Err("missing readable level");
    }
    serial
        .acknowledge_output(CAPACITY + 1)
        .map_err(|_| "drain acknowledgement")?;
    if serial.observed_signals() & readable != 0 {
        return Err("readable after drain");
    }
    serial.publish_guest_output(b'n');
    if serial.observed_signals() & readable == 0 {
        return Err("lost rearmed notification");
    }
    // An old but valid acknowledgement must not clear newly published data.
    serial
        .acknowledge_output(CAPACITY + 1)
        .map_err(|_| "old acknowledgement")?;
    if serial.observed_signals() & readable == 0 {
        return Err("acknowledgement lost new data");
    }
    // Process handle-table retirement invokes this callback on runtime loss.
    serial.on_zero_active_handles(&mut ObjectRetirement::new());
    serial
        .acknowledge_output(CAPACITY + 2)
        .map_err(|_| "closed acknowledgement")?;
    serial.publish_guest_output(b'!');
    if producer.load(Ordering::Acquire) != CAPACITY + 2 {
        return Err("output after runtime ownership loss");
    }
    if domain.usage().committed(ResourceKind::PinnedPages) == 0 {
        return Err("backing retired before device owner");
    }
    // No reference derived above is accessed after final device retirement.
    drop(serial);
    if domain.usage().committed(ResourceKind::PinnedPages) != 0
        || domain.usage().committed(ResourceKind::CommittedPages) != 0
    {
        return Err("registered pages leaked");
    }
    Ok(())
}
