// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Userspace-exportable task identity and construction authorities.

use crate::authority::Rights;
use hyper::mm::WeakFallibleArc;
use hyper::sync::PublishedOnce;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceError,
    crate::accounting::ResourceKind,
};
use {
    crate::object::KernelObject, crate::object::KernelService, crate::object::ObjectCreationError,
    crate::object::ObjectKind, crate::object::ObjectPublication, crate::object::PublishableRef,
    crate::object::SignalMask, crate::object::SignalSource, crate::object::SignalState,
    crate::object::TransferClass, crate::object::object_allocation_size, crate::object::private,
};

use super::owner::ProcessInner;
use super::{Process, ProcessSnapshot, TaskGroup, TaskGroupError, TerminalReason};
use crate::accounting::ResourceDomainObject;

/// Failure while preparing an accounted task capability object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskObjectError {
    AlreadyPublished,
    AllocationSize,
    Object(ObjectCreationError),
    Resource(ResourceError),
    TaskGroup(TaskGroupError),
}

impl From<ObjectCreationError> for TaskObjectError {
    fn from(error: ObjectCreationError) -> Self {
        Self::Object(error)
    }
}

impl From<TaskGroupError> for TaskObjectError {
    fn from(error: TaskGroupError) -> Self {
        Self::TaskGroup(error)
    }
}

impl From<ResourceError> for TaskObjectError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

/// Canonical userspace authority over one existing Process lifecycle.
///
/// The contained Process owner is the same owner used by `TaskGroup` membership
/// and scheduler execution. The object adds a KOID and handle rights; it does
/// not create a second task lifecycle or an independently reclaimable task.
pub(crate) struct ProcessObject {
    process: WeakFallibleArc<ProcessInner>,
    final_snapshot: PublishedOnce<ProcessSnapshot>,
    signals: SignalState,
    _object_charge: CommittedCharge,
}

impl ProcessObject {
    /// Authority returned to the parent by a successful builder start.
    pub(crate) const SUPERVISOR_RIGHTS: Rights = Rights::TRANSFER
        .union(Rights::WAIT)
        .union(Rights::INSPECT)
        .union(Rights::REQUEST_STOP);

    fn try_new(process: &Process) -> Result<Self, TaskObjectError> {
        let charge = reserve_object_charge::<Self>(&process.resource_domain())?;
        Ok(Self {
            process: process.inner.downgrade(),
            final_snapshot: PublishedOnce::new(),
            signals: SignalState::new(),
            _object_charge: charge,
        })
    }

    pub(crate) fn snapshot(&self) -> ProcessSnapshot {
        if let Some(inner) = self.process.upgrade() {
            return Process { inner }.snapshot();
        }
        match self.final_snapshot.get() {
            Some(snapshot) => *snapshot,
            None => hyper::debug::invariant_failure("process::objects::snapshot invariant"),
        }
    }

    pub(crate) fn request_stop(&self, reason: TerminalReason) {
        if let Some(inner) = self.process.upgrade() {
            let _ = (Process { inner }).request_stop(reason);
        }
    }

    pub(super) fn publish_stopped(&self) {
        if self
            .signals
            .update(SignalMask::EMPTY, Process::TERMINATED)
            .is_err()
        {
            hyper::debug::invariant_failure("process::objects::publish_stopped invariant");
        }
    }

    pub(super) fn publish_final_snapshot(&self, snapshot: ProcessSnapshot) {
        if self.final_snapshot.publish(snapshot).is_err() {
            hyper::debug::invariant_failure("process::objects::publish_final_snapshot invariant");
        }
    }

    /// Constructs the canonical service owner for this `Process`.
    pub(crate) fn try_service(
        process: &Process,
    ) -> Result<PublishableRef<Self, KernelService>, TaskObjectError> {
        Ok(PublishableRef::try_new(Self::try_new(process)?)?)
    }
}

impl private::Sealed for ProcessObject {}
impl private::UserExportable for ProcessObject {}

impl KernelObject for ProcessObject {
    const KIND: ObjectKind = ObjectKind::PROCESS;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::WAIT)
        .union(Rights::INSPECT)
        .union(Rights::START)
        .union(Rights::REQUEST_STOP)
        .union(Rights::CREATE_THREAD);

    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(SignalSource::new(&self.signals, Process::SUPPORTED_SIGNALS))
    }
}

const _: () = assert!(
    ProcessObject::SUPERVISOR_RIGHTS.bits()
        == hyper::abi::native::HYPER_NATIVE_RIGHT_TRANSFER
            | hyper::abi::native::HYPER_NATIVE_RIGHT_WAIT
            | hyper::abi::native::HYPER_NATIVE_RIGHT_INSPECT
            | hyper::abi::native::HYPER_NATIVE_RIGHT_REQUEST_STOP
);

/// Userspace authority over grouped Process lifecycle operations.
pub(crate) struct TaskGroupObject {
    group: TaskGroup,
    _object_charge: CommittedCharge,
}

impl TaskGroupObject {
    fn try_new(group: TaskGroup) -> Result<Self, TaskObjectError> {
        let charge = reserve_object_charge::<Self>(&group.resource_domain())?;
        Ok(Self {
            group,
            _object_charge: charge,
        })
    }

    pub(crate) const fn group(&self) -> &TaskGroup {
        &self.group
    }

    /// Constructs the single userspace object identity for this `TaskGroup`.
    pub(crate) fn try_publication(
        group: TaskGroup,
    ) -> Result<ObjectPublication<Self>, TaskObjectError> {
        if !group.claim_object_publication() {
            return Err(TaskObjectError::AlreadyPublished);
        }
        let result = Self::try_new(group.clone())
            .and_then(|payload| ObjectPublication::try_new(payload).map_err(Into::into));
        if result.is_err() {
            group.abort_object_publication();
        }
        result
    }
}

impl private::Sealed for TaskGroupObject {}
impl private::UserExportable for TaskGroupObject {}

impl KernelObject for TaskGroupObject {
    const KIND: ObjectKind = ObjectKind::TASK_GROUP;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::WAIT)
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::REQUEST_STOP)
        .union(Rights::TASK_GROUP_ATTACH_PROCESS);

    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(self.group.signal_source())
    }

    fn on_zero_active_handles(&self, _retirement: &mut crate::object::ObjectRetirement) {
        // A TaskGroup handle is an ownership lease, not merely an inspector.
        // Process teardown closes handles even when userspace destructors do
        // not run, so the last manager owner reliably initiates stop for every
        // member and prevents orphaned service processes.
        let _ = self.group.request_stop();
    }
}

/// Stateless authority required to construct task hierarchy objects.
///
/// `ResourceDomain` and `TaskGroup` handles remain separate arguments to creation
/// operations. Possessing this factory never grants access to either object;
/// all participating handles must independently resolve with their required
/// rights before construction begins.
pub(crate) struct TaskFactory {
    _object_charge: CommittedCharge,
}

impl TaskFactory {
    pub(crate) fn try_new(sponsor: &ResourceDomain) -> Result<Self, TaskObjectError> {
        Ok(Self {
            _object_charge: reserve_object_charge::<Self>(sponsor)?,
        })
    }

    pub(crate) fn try_create_task_group(
        &self,
        domain: &ResourceDomainObject,
    ) -> Result<ObjectPublication<TaskGroupObject>, TaskObjectError> {
        TaskGroupObject::try_publication(TaskGroup::try_new(domain.domain())?)
    }
}

impl private::Sealed for TaskFactory {}
impl private::UserExportable for TaskFactory {}

impl KernelObject for TaskFactory {
    const KIND: ObjectKind = ObjectKind::TASK_FACTORY;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::CREATE_PROCESS)
        .union(Rights::CREATE_TASK_GROUP)
        .union(Rights::LOCK_MEMORY)
        .union(Rights::RETAIN_PROCESS)
        .union(crate::task::scheduling_authority::SCHEDULING_RIGHTS);
}

fn reserve_object_charge<T: KernelObject>(
    domain: &ResourceDomain,
) -> Result<CommittedCharge, TaskObjectError> {
    let bytes = object_allocation_size::<T>()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(TaskObjectError::AllocationSize)?;
    Ok(domain
        .reserve(
            ResourceAmount::ZERO
                .with(ResourceKind::KernelObjects, 1)
                .with(ResourceKind::KernelMemoryBytes, bytes),
        )?
        .commit())
}
