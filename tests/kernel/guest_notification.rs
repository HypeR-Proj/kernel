// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Deterministic interleavings at the production saved-state/publication cut.
//! The VMs remain dormant: endpoint reconcile state cannot be consumed by a
//! running guest, so a lost publication cannot hide behind an unrelated exit.

use super::{InstalledMachine, Notification, ResourceDomain, SignalMask, VmBinding};
use hyper::mm::FallibleArc;
use {crate::vm::registry, crate::vm::registry::PreparedVm};
use {
    hyper_vm_models::exit::AccessWidth, hyper_vm_models::exit::GuestPhysicalAddress,
    hyper_vm_models::exit::MmioAccess, hyper_vm_models::exit::MmioAction,
    hyper_vm_models::exit::MmioOperation,
};

type Result<T = ()> = core::result::Result<T, &'static str>;
const BASE: u64 = 0x0a00_0000;
const IRQ: u32 = 64;

pub(super) fn run(front: PreparedVm, back: PreparedVm, domain: &ResourceDomain) -> Result {
    let front = front
        .install()
        .map_err(|_| "install front VM")?
        .publish_handle_lifecycle();
    let back = back
        .install()
        .map(|installed| installed.publish_handle_lifecycle())
        .map_err(|_| "install back VM");
    let result = match &back {
        Ok(back) => (|| {
            exercise(&front, back, domain)?;
            let source = crate::vm::io::Interrupt::prepare(front.clone(), 122, domain)
                .map_err(|_| "retirement source")?;
            source.install().map_err(|_| "retirement source install")?;
            source
                .set_level(true)
                .map_err(|_| "retirement source assert")?;
            Ok(source)
        })(),
        Err(error) => Err(*error),
    };
    // Every exit retires both dormant vCPU scheduler objects. The caller also
    // checks resource release and scheduler quiescence before its next test.
    InstalledMachine::request_stop(&front);
    if let Ok(back) = &back {
        InstalledMachine::request_stop(back);
    }
    let stopped = hyper::abi::native::HYPER_NATIVE_VIRTUAL_MACHINE_PHASE_STOPPED as u32;
    if !crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, crate::task::SleepError>(
            front.snapshot().phase == stopped
                && back
                    .as_ref()
                    .map_or(true, |back| back.snapshot().phase == stopped),
        )
    })
    .map_err(|_| "notification VM retirement wait")?
    {
        return Err("notification VM retirement timeout");
    }
    if let Ok(source) = &result
        && source.set_level(true).is_ok()
    {
        return Err("retired VM accepted source update");
    }
    result.map(|_| ())
}

fn exercise(
    front: &FallibleArc<InstalledMachine>,
    back: &FallibleArc<InstalledMachine>,
    domain: &ResourceDomain,
) -> Result {
    let front_binding = registry::acquire_binding(front.io_install_id().map_err(|_| "front ID")?)
        .map_err(|_| "front binding")?;
    let back_binding = registry::acquire_binding(back.io_install_id().map_err(|_| "back ID")?)
        .map_err(|_| "back binding")?;
    front
        .register_mmio(BASE, 8192, 1)
        .map_err(|_| "frontend MMIO reservation")?;
    let notification = install(front, back, domain)?;
    // Controller construction may leave initial reconcile work. Consume it
    // before the interleavings, rather than letting it satisfy assertions.
    for binding in [&front_binding, &back_binding] {
        binding.publish_changed_interrupts();
        binding
            .claim_detached_interrupt_reconcile(0)
            .map_err(|_| "initial reconcile")?;
    }
    let epoch = notification.control(1).map_err(|_| "enable notification")?;
    verify_guest_interrupt(front, domain)?;
    front_binding
        .claim_detached_interrupt_reconcile(0)
        .map_err(|_| "source reconcile cleanup")?;
    verify_noop(&notification, &back_binding)?;
    verify_ack(&notification, &front_binding, epoch)?;
    verify_pci(&notification, front, &front_binding, &back_binding, epoch)?;
    verify_replacement(&notification, front, back, &back_binding, domain)?;
    verify_native_close(back, &back_binding, domain)
}

fn install(
    front: &FallibleArc<InstalledMachine>,
    back: &FallibleArc<InstalledMachine>,
    domain: &ResourceDomain,
) -> Result<Notification> {
    let notification = Notification::prepare(front, back, BASE, BASE, IRQ, IRQ, domain)
        .map_err(|_| "prepare notification")?;
    notification
        .install(front, back)
        .map_err(|_| "install notification")?;
    Ok(notification)
}

fn reconcile(binding: &VmBinding, expected: bool) -> Result {
    if binding
        .claim_detached_interrupt_reconcile(0)
        .map_err(|_| "take reconcile")?
        != expected
    {
        return Err("notification reconcile publication mismatch");
    }
    Ok(())
}

fn read(notification: &Notification, offset: u64) -> MmioAction {
    notification.shared.back_mmio(MmioAccess::new(
        GuestPhysicalAddress::new(BASE + offset),
        AccessWidth::Word,
        MmioOperation::Read,
    ))
}

fn verify_noop(notification: &Notification, back: &VmBinding) -> Result {
    let (_, pending) = notification.shared.update_saved(|state| state.kick(0));
    // These later operations must neither lose the first producer's work nor
    // need another line transition just to add a distinct queue bit.
    notification.shared.mutate(|state| {
        state.kick(0);
        state.kick(1);
    });
    if read(notification, 0x0c) != MmioAction::CompleteRead(1) {
        return Err("notification enabled snapshot");
    }
    reconcile(back, false)?;
    pending.publish();
    reconcile(back, true)?;
    if read(notification, 0x10) != MmioAction::CompleteRead(3) {
        return Err("coalescing lost a queue kick");
    }
    reconcile(back, true)?;
    // A fresh producer after drain must raise the saved line again. This also
    // detects a missing deassertion hidden by the first producer's dirty bit.
    notification.shared.mutate(|state| state.kick(2));
    reconcile(back, true)?;
    if read(notification, 0x10) != MmioAction::CompleteRead(4) {
        return Err("notification did not rearm after drain");
    }
    reconcile(back, true)
}

fn verify_ack(notification: &Notification, front: &VmBinding, epoch: u32) -> Result {
    let (_, pending) = notification
        .shared
        .update_saved(|state| state.call((u64::from(epoch) << 32) | 1));
    if notification.shared.front_mmio(MmioAccess::new(
        GuestPhysicalAddress::new(BASE + 0x64),
        AccessWidth::Word,
        MmioOperation::Write(1),
    )) != Some(MmioAction::CompleteWrite)
    {
        return Err("notification acknowledgement");
    }
    reconcile(front, true)?;
    // A delayed publisher consumes current controller changes, not a captured
    // old asserted value which would resurrect the acknowledged interrupt.
    pending.publish();
    reconcile(front, false)?;
    if notification.shared.front_mmio(MmioAccess::new(
        GuestPhysicalAddress::new(BASE + 0x60),
        AccessWidth::Word,
        MmioOperation::Read,
    )) != Some(MmioAction::CompleteRead(0))
    {
        return Err("acknowledged completion reappeared");
    }
    notification
        .shared
        .mutate(|state| state.call((u64::from(epoch) << 32) | 1));
    reconcile(front, true)?;
    notification.shared.mutate(|state| state.ack(1));
    reconcile(front, true)
}

fn verify_replacement(
    old: &Notification,
    front: &FallibleArc<InstalledMachine>,
    back: &FallibleArc<InstalledMachine>,
    binding: &VmBinding,
    domain: &ResourceDomain,
) -> Result {
    let (_, old_pending) = old.shared.update_saved(|state| state.kick(0));
    old.disconnect()
        .map_err(|_| "disconnect old notification")?;
    reconcile(binding, true)?;
    let replacement = install(front, back, domain)?;
    replacement.control(1).map_err(|_| "enable replacement")?;
    let (_, new_pending) = replacement.shared.update_saved(|state| state.kick(2));
    // An already cloned old route must not rewrite a reused controller line.
    old.shared.mutate(|state| state.kick(1));
    old.shared.close();
    if read(old, 0x0c) != MmioAction::CompleteRead(0) {
        return Err("detached notification remained enabled");
    }
    reconcile(binding, false)?;
    old_pending.publish();
    reconcile(binding, true)?;
    new_pending.publish();
    reconcile(binding, false)?;
    if read(&replacement, 0x10) != MmioAction::CompleteRead(4) {
        return Err("old route changed replacement kicks");
    }
    reconcile(binding, true)?;
    replacement
        .disconnect()
        .map_err(|_| "disconnect replacement")?;
    Ok(())
}

fn verify_native_close(
    back: &FallibleArc<InstalledMachine>,
    binding: &VmBinding,
    domain: &ResourceDomain,
) -> Result {
    let native = Notification::prepare_native(back, BASE, IRQ, domain)
        .map_err(|_| "prepare Native notification")?;
    native
        .install_native(back)
        .map_err(|_| "install Native notification")?;
    let epoch = native
        .control(1)
        .map_err(|_| "enable Native notification")?;
    native
        .shared
        .mutate(|state| state.call((u64::from(epoch) << 32) | 1));
    native
        .shared
        .mutate(|state| state.call((u64::from(epoch) << 32) | 2));
    let signals = SignalMask::from_trusted_bits(7);
    if native
        .shared
        .signals
        .observe(signals)
        .map(|s| s.signals().bits())
        != Some(6)
    {
        return Err("Native completion bit lost while level stayed high");
    }
    let (_, pending) = native.shared.update_saved(|state| state.kick(0));
    native.close_native();
    reconcile(binding, true)?;
    pending.publish();
    reconcile(binding, false)?;
    if native
        .shared
        .signals
        .observe(signals)
        .map(|s| s.signals().bits())
        != Some(1)
        || read(&native, 0x10) != MmioAction::CompleteRead(0)
    {
        return Err("Native close lost terminal state");
    }
    Ok(())
}

fn verify_pci(
    notification: &Notification,
    machine: &InstalledMachine,
    front: &VmBinding,
    back: &VmBinding,
    epoch: u32,
) -> Result {
    let relocated = BASE + 4096;
    machine
        .configure_notification(BASE, relocated, 1 | 2 | 8)
        .map_err(|_| "configure PCI notification")?;
    if machine.configure_notification(BASE, BASE + 8192, 3).is_ok() {
        return Err("PCI route escaped userspace reservation");
    }
    let access = |address, width, operation| {
        MmioAccess::new(GuestPhysicalAddress::new(address), width, operation)
    };
    if front
        .route_io_mmio(access(
            BASE + 0x50,
            AccessWidth::HalfWord,
            MmioOperation::Write(1),
        ))
        .is_some()
    {
        return Err("relocated BAR retained old doorbell");
    }
    if front.route_io_mmio(access(
        relocated + 0x50,
        AccessWidth::HalfWord,
        MmioOperation::Write(1),
    )) != Some(MmioAction::CompleteWrite)
    {
        return Err("PCI 16-bit queue notify");
    }
    if read(notification, 0x10) != MmioAction::CompleteRead(2) {
        return Err("PCI queue kick lost");
    }
    back.publish_changed_interrupts();
    back.claim_detached_interrupt_reconcile(0)
        .map_err(|_| "PCI kick reconcile")?;
    notification
        .shared
        .mutate(|state| state.call((u64::from(epoch) << 32) | 3));
    front
        .claim_detached_interrupt_reconcile(0)
        .map_err(|_| "PCI interrupt reconcile")?;
    machine
        .configure_notification(BASE, relocated, 1 | 2 | 4 | 8)
        .map_err(|_| "mask INTx")?;
    if notification
        .shared
        .state
        .with(|state| state.front_irq() || state.status != 3)
    {
        return Err("INTx mask discarded status");
    }
    machine
        .configure_notification(BASE, relocated, 1 | 2 | 8)
        .map_err(|_| "unmask INTx")?;
    if !notification.shared.state.with(|state| state.front_irq()) {
        return Err("INTx unmask lost pending level");
    }
    if front.route_io_mmio(access(
        relocated + 0x60,
        AccessWidth::Byte,
        MmioOperation::Read,
    )) != Some(MmioAction::CompleteRead(3))
        || front.route_io_mmio(access(
            relocated + 0x60,
            AccessWidth::Byte,
            MmioOperation::Read,
        )) != Some(MmioAction::CompleteRead(0))
    {
        return Err("PCI ISR did not consume status exactly once");
    }
    machine
        .configure_notification(BASE, relocated, 1)
        .map_err(|_| "disable PCI decode")?;
    if front
        .route_io_mmio(access(
            relocated + 0x50,
            AccessWidth::HalfWord,
            MmioOperation::Write(1),
        ))
        .is_some()
    {
        return Err("disabled PCI memory remained decoded");
    }
    // The explicit transport fallback bypasses MEM decoding, but not BME or
    // connection lifetime. Both access paths share interrupt/kick state.
    machine
        .access_notification(BASE, 2, 1)
        .map_err(|_| "disabled BME fallback")?;
    if read(notification, 0x10) != MmioAction::CompleteRead(0) {
        return Err("configuration access bypassed bus-master disable");
    }
    machine
        .configure_notification(BASE, relocated, 1 | 8)
        .map_err(|_| "enable fallback BME")?;
    machine
        .access_notification(BASE, 2, 1)
        .map_err(|_| "fallback notify")?;
    if read(notification, 0x10) != MmioAction::CompleteRead(2) {
        return Err("configuration notification lost while MEM disabled");
    }
    notification
        .shared
        .mutate(|state| state.call((u64::from(epoch) << 32) | 3));
    if machine
        .access_notification(BASE, 0, 0)
        .map_err(|_| "fallback status")?
        != 3
        || machine
            .access_notification(BASE, 1, 0)
            .map_err(|_| "fallback ISR")?
            != 3
        || machine
            .access_notification(BASE, 0, 0)
            .map_err(|_| "fallback cleared status")?
            != 0
        || machine.access_notification(BASE, 3, 0).is_ok()
        || machine.access_notification(BASE, 2, 32).is_ok()
        || machine.access_notification(BASE + 4096, 0, 0).is_ok()
    {
        return Err("configuration notification contract");
    }
    machine
        .configure_notification(BASE, BASE, 0)
        .map_err(|_| "restore MMIO transport")?;
    for binding in [front, back] {
        binding.publish_changed_interrupts();
        binding
            .claim_detached_interrupt_reconcile(0)
            .map_err(|_| "PCI final reconcile")?;
    }
    Ok(())
}

fn verify_guest_interrupt(
    machine: &FallibleArc<InstalledMachine>,
    domain: &ResourceDomain,
) -> Result {
    use crate::vm::io::Interrupt;
    use {crate::object::KernelObject, crate::object::ObjectRetirement};
    if Interrupt::prepare(machine.clone(), 33, domain).is_ok() {
        return Err("userspace reserved built-in serial IRQ");
    }
    let collision =
        Interrupt::prepare(machine.clone(), IRQ, domain).map_err(|_| "collision source")?;
    if collision.install().is_ok() {
        return Err("notification IRQ collision admitted");
    }
    let source = Interrupt::prepare(machine.clone(), 120, domain).map_err(|_| "source")?;
    source.install().map_err(|_| "source installation")?;
    source.set_level(true).map_err(|_| "source assert")?;
    source.set_level(false).map_err(|_| "source deassert")?;
    let duplicate =
        Interrupt::prepare(machine.clone(), 120, domain).map_err(|_| "duplicate source")?;
    if duplicate.install().is_ok() {
        return Err("duplicate source admitted");
    }
    source.on_zero_active_handles(&mut ObjectRetirement::new());
    if source.set_level(true).is_ok() || duplicate.install().is_ok() {
        return Err("closed source was reasserted or its reservation reused");
    }
    Ok(())
}
