// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Authority checks and bounded instruction-publication scheduling points.

use super::vmo::{PreparedExecutableView, PreparedExecutableVmo, PrivateView};
use super::{
    DomainAccount, ExecutableAuthority, ExecutableVmo, KernelPageBackend, MemoryObjectError,
    MemoryServiceError, VmoObject,
};
use crate::capability::{HandleValue, InvocationReceiver, Rights};
use crate::process::{Process, ProcessError};
use crate::task::scheduler;
use hyper::mm::FallibleArc;

#[derive(Debug)]
pub(crate) enum PublicationError {
    Vmo(super::VmoError<super::KernelPageError, crate::accounting::ResourceError>),
    Scheduler(scheduler::Error),
}

impl From<PublicationError> for MemoryServiceError {
    fn from(error: PublicationError) -> Self {
        match error {
            PublicationError::Vmo(error) => Self::MemoryObject(MemoryObjectError::Vmo(error)),
            PublicationError::Scheduler(error) => Self::Scheduler(error),
        }
    }
}

pub(crate) fn executable_publish(
    process: &Process,
    authority: &InvocationReceiver,
    source: HandleValue,
) -> Result<(HandleValue, u64), MemoryServiceError> {
    let authority = authority
        .resolve::<ExecutableAuthority>(Rights::CREATE_EXECUTABLE)
        .map_err(ProcessError::from)?;
    let source = process.resolve_handle::<VmoObject>(source, Rights::READ)?;
    let sponsor = process.resource_domain();
    // Freeze/copy may allocate and reclaim; neither it nor object publication
    // runs with preemption disabled. No handle observes an intermediate copy.
    let prepared = source
        .object()
        .prepare_executable_snapshot(authority.object(), &sponsor)?;
    let executable = publish_snapshot(prepared)?;
    let object = VmoObject::from_executable(executable, &sponsor)?;
    let size = object.size();
    let rights = Rights::READ
        .union(Rights::MAP)
        .union(Rights::EXECUTE)
        .union(Rights::DUPLICATE)
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT);
    Ok((process.create_object(object, rights)?, size))
}

pub(crate) fn publish_snapshot(
    mut prepared: PreparedExecutableVmo<KernelPageBackend, DomainAccount>,
) -> Result<ExecutableVmo<KernelPageBackend, DomainAccount>, PublicationError> {
    loop {
        let pin = scheduler::preempt_disable().map_err(PublicationError::Scheduler)?;
        let result = prepared.publish_next(&pin);
        // A pending reschedule must be observed between batches, including the
        // error path; immediately repinning would defeat the bounded work unit.
        scheduler::preempt_enable_and_reschedule(pin).map_err(PublicationError::Scheduler)?;
        if result.map_err(PublicationError::Vmo)? {
            return prepared.finish().map_err(PublicationError::Vmo);
        }
    }
}

pub(crate) fn publish_view(
    mut prepared: PreparedExecutableView<KernelPageBackend, DomainAccount>,
) -> Result<FallibleArc<PrivateView<KernelPageBackend, DomainAccount>>, PublicationError> {
    loop {
        let pin = scheduler::preempt_disable().map_err(PublicationError::Scheduler)?;
        let result = prepared.publish_next(&pin);
        scheduler::preempt_enable_and_reschedule(pin).map_err(PublicationError::Scheduler)?;
        if result.map_err(PublicationError::Vmo)? {
            return prepared.finish().map_err(PublicationError::Vmo);
        }
    }
}
