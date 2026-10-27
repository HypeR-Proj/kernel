// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native capability preparation and publication for guest I/O mechanisms.

use super::{Error, Mailbox, Notification};
use {
    crate::capability::HandleFlags, crate::capability::HandleValue,
    crate::capability::InvocationReceiver, crate::capability::PreparedHandle,
    crate::capability::Rights,
};
use {crate::object::ObjectPublication, crate::object::UserExportableObject};
use {crate::process::Process, crate::process::ProcessError};
use {crate::vm::objects::Error as ObjectError, crate::vm::objects::VirtualMachineObject};

fn publish<T: UserExportableObject>(
    process: &Process,
    payload: T,
    install: impl FnOnce() -> Result<(), Error>,
) -> Result<HandleValue, Error> {
    let output = process.reserve_handles::<1>()?;
    let prepare = || {
        let publication = ObjectPublication::try_new(payload).map_err(ObjectError::from)?;
        let prepared = PreparedHandle::try_from_new_object(
            publication,
            T::SUPPORTED_RIGHTS,
            HandleFlags::NONE,
        )
        .map_err(ProcessError::from)?;
        install()?;
        Ok::<_, Error>(prepared)
    };
    let prepared = match prepare() {
        Ok(prepared) => prepared,
        Err(error) => {
            process.abort_handles(output);
            return Err(error);
        }
    };
    // Once topology is installed, a concurrent process teardown closes the
    // prepared handle and therefore the bridge. Its immutable MMIO reservation
    // remains VM-owned until VM retirement, never a dangling userspace owner.
    process
        .publish_handles(output, [prepared])
        .map(|values| values[0])
        .map_err(|failure| failure.error.into())
}

pub(crate) fn create_mailbox(
    process: &Process,
    machine: &InvocationReceiver,
    base: u64,
    irq: u32,
) -> Result<HandleValue, Error> {
    let machine = machine
        .resolve::<VirtualMachineObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let owner = machine.object().owner();
    let mailbox = Mailbox::prepare(&owner, base, irq, &process.resource_domain())?;
    let installer = mailbox.clone();
    publish(process, mailbox, || installer.install(&owner))
}

pub(crate) fn send_mailbox(mailbox: &InvocationReceiver, bytes: &[u8]) -> Result<(), Error> {
    let mailbox = mailbox
        .resolve::<Mailbox>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    mailbox.object().send(bytes)
}

pub(crate) fn receive_mailbox(
    mailbox: &InvocationReceiver,
    mut copy: impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<usize, Error> {
    let mailbox = mailbox
        .resolve::<Mailbox>(Rights::READ)
        .map_err(crate::process::ProcessError::from)?;
    let claim = mailbox.object().claim()?;
    let length = claim.bytes().len();
    copy(claim.bytes())?;
    claim.commit();
    Ok(length)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_notification(
    process: &Process,
    front: &InvocationReceiver,
    back: HandleValue,
    front_base: u64,
    back_base: u64,
    front_irq: u32,
    back_irq: u32,
) -> Result<HandleValue, Error> {
    let front = front
        .resolve::<VirtualMachineObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let back = process.resolve_handle::<VirtualMachineObject>(back, Rights::WRITE)?;
    let front = front.object().owner();
    let back = back.object().owner();
    let notification = Notification::prepare(
        &front,
        &back,
        front_base,
        back_base,
        front_irq,
        back_irq,
        &process.resource_domain(),
    )?;
    let installer = notification.clone();
    publish(process, notification, || installer.install(&front, &back))
}

pub(crate) fn control_notification(
    notification: &InvocationReceiver,
    operation: u32,
) -> Result<u32, Error> {
    let notification = notification
        .resolve::<Notification>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    notification.object().control(operation)
}

/// Reconfigure only the frontend decode of an already admitted connection.
/// VM WRITE cannot change its backend, interrupt assignment, epoch or DMA lease.
pub(crate) fn configure_notification(
    machine: &InvocationReceiver,
    original: u64,
    base: u64,
    flags: u32,
) -> Result<(), Error> {
    let machine = machine
        .resolve::<VirtualMachineObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    machine
        .object()
        .owner()
        .configure_notification(original, base, flags)
}

pub(crate) fn access_notification(
    machine: &InvocationReceiver,
    original: u64,
    operation: u32,
    value: u32,
) -> Result<u32, Error> {
    machine
        .resolve::<VirtualMachineObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?
        .object()
        .owner()
        .access_notification(original, operation, value)
}

/// Reserves a line until VM retirement, even after its userspace owner closes.
pub(crate) fn create_interrupt(
    process: &Process,
    machine: &InvocationReceiver,
    irq: u32,
) -> Result<HandleValue, Error> {
    let machine = machine
        .resolve::<VirtualMachineObject>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?;
    let interrupt =
        super::Interrupt::prepare(machine.object().owner(), irq, &process.resource_domain())?;
    let installer = interrupt.clone();
    publish(process, interrupt, || installer.install())
}
pub(crate) fn set_interrupt_level(
    interrupt: &InvocationReceiver,
    asserted: bool,
) -> Result<(), Error> {
    interrupt
        .resolve::<super::Interrupt>(Rights::WRITE)
        .map_err(crate::process::ProcessError::from)?
        .object()
        .set_level(asserted)
}
