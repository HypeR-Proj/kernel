// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime proof that global object discovery neither owns nor resurrects objects.

use crate::kernel::accounting::{ResourceDomain, ResourceKind, ResourceLimits};
use crate::kernel::object::{Event, ObjectKind, ObjectScanCursor, PublishableRef};
use crate::kernel::task::{ThreadObjectRegistryPhase, ThreadObjectScanCursor};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Construction,
    CurrentThread,
    MissingLiveObject,
    MissingSchedulerObject,
    MissingSchedulerRelation,
    ObservationChangedOwnership,
    ReaperDidNotRun,
    RetainedDeadObject,
    Sleep(crate::kernel::task::SleepError),
}

impl From<crate::kernel::task::SleepError> for Error {
    fn from(error: crate::kernel::task::SleepError) -> Self {
        Self::Sleep(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let event = Event::try_new(&domain).map_err(|_| Error::Construction)?;
    let object = PublishableRef::try_new(event).map_err(|_| Error::Construction)?;
    let koid = object.snapshot().koid;
    let before = object.snapshot();
    if !contains(koid) {
        return Err(Error::MissingLiveObject);
    }
    let after = object.snapshot();
    if after != before {
        return Err(Error::ObservationChangedOwnership);
    }
    drop(object);
    if contains(koid) {
        return Err(Error::RetainedDeadObject);
    }
    verify_final_reap()?;
    verify_current_thread_object()?;
    drop(domain);
    Ok(())
}

fn verify_final_reap() -> Result<(), Error> {
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let before = domain.usage().committed(ResourceKind::KernelObjects);
    let probe = Event::try_new(&domain).map_err(|_| Error::Construction)?;
    let probe = PublishableRef::try_new(probe).map_err(|_| Error::Construction)?;
    let charged = domain.usage().committed(ResourceKind::KernelObjects);
    if charged != before.checked_add(1).ok_or(Error::ReaperDidNotRun)? {
        return Err(Error::Construction);
    }
    drop(probe);
    if crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, Error>(domain.usage().committed(ResourceKind::KernelObjects) == before),
    )? {
        Ok(())
    } else {
        Err(Error::ReaperDidNotRun)
    }
}

fn verify_current_thread_object() -> Result<(), Error> {
    let thread =
        crate::kernel::task::scheduler::current_thread_id().map_err(|_| Error::CurrentThread)?;
    let object = crate::kernel::task::scheduler::thread_object_snapshot(thread)
        .map_err(|_| Error::MissingSchedulerObject)?;
    if object.object.kind != ObjectKind::THREAD
        || object.object.references.scheduler == 0
        || !contains(object.object.koid)
    {
        return Err(Error::MissingSchedulerObject);
    }

    let mut cursor = Some(ThreadObjectScanCursor::start());
    while let Some(position) = cursor {
        let page = crate::kernel::task::scheduler::scan_thread_objects(position)
            .map_err(|_| Error::MissingSchedulerRelation)?;
        if page.entries().any(|entry| {
            entry.thread == thread
                && entry.object.object.koid == object.object.koid
                && entry.phase == ThreadObjectRegistryPhase::Resident
        }) {
            return Ok(());
        }
        cursor = page.next();
    }
    Err(Error::MissingSchedulerRelation)
}

fn contains(target: crate::kernel::object::Koid) -> bool {
    let mut cursor = Some(ObjectScanCursor::start());
    while let Some(position) = cursor {
        let page = crate::kernel::object::scan(position);
        if page.entries().any(|snapshot| snapshot.koid == target) {
            return true;
        }
        cursor = page.next();
    }
    false
}
