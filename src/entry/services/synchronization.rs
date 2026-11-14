// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Capability validation for synchronization domains.

use super::ProcessServices;
use crate::abi::native::{SyncWaitRequest, SynchronizationServices};
use crate::capability::{HandleValue, InvocationReceiver, Rights};
use crate::fs::vfs::FileObject;
use crate::mm::user_space::VmoObject;
use crate::object::ObjectKind;
use crate::object::{Event, KernelObject, PreparedCancellation};
use crate::process::sync_domain::{OwnerState, SyncDomain};
use crate::process::sync_wait::{self, Error, Outcome};
use crate::process::{ProcessError, UserThreadPhase};

impl SynchronizationServices for ProcessServices<'_> {
    fn sync_domain_create(&self) -> Result<HandleValue, Error> {
        let domain = SyncDomain::try_new(&self.process.resource_domain())?;
        Ok(self
            .process
            .create_object(domain, SyncDomain::SUPPORTED_RIGHTS)?)
    }
    fn sync_domain_bind(
        &self,
        receiver: &InvocationReceiver,
        source: HandleValue,
        offset: u64,
        length: u64,
    ) -> Result<(), Error> {
        let domain = receiver
            .resolve::<SyncDomain>(Rights::SYNC_CONFIGURE)
            .map_err(ProcessError::from)?
            .into_operation_pin();
        let source = self.process.resolve_invocation(source)?;
        let rights = Rights::WRITE.union(Rights::SYNC_CONFIGURE);
        if source.kind() == ObjectKind::FILE {
            let file = source
                .resolve::<FileObject>(rights)
                .map_err(ProcessError::from)?;
            let sponsor = self.process.resource_domain();
            let binding = SyncDomain::prepare_binding(&domain, offset, length, &sponsor)?;
            if !file
                .object()
                .bind_shared_sync(binding, &sponsor)
                .map_err(file_error)?
            {
                return Err(crate::process::sync_domain::Error::ConflictingBinding.into());
            }
        } else {
            let vmo = source
                .resolve::<VmoObject>(rights)
                .map_err(ProcessError::from)?;
            SyncDomain::bind(
                &domain,
                vmo.object(),
                offset,
                length,
                &self.process.resource_domain(),
            )?;
        }
        Ok(())
    }
    fn sync_domain_join(&self, receiver: &InvocationReceiver) -> Result<u64, Error> {
        let domain = receiver
            .resolve::<SyncDomain>(Rights::SYNC_JOIN)
            .map_err(ProcessError::from)?
            .into_operation_pin();
        Ok(SyncDomain::join(
            &domain,
            self.thread,
            &self.process.resource_domain(),
        )?)
    }
    fn sync_domain_owner_query(
        &self,
        receiver: &InvocationReceiver,
        owner: u64,
    ) -> Result<u64, Error> {
        let domain = receiver
            .resolve::<SyncDomain>(Rights::INSPECT)
            .map_err(ProcessError::from)?;
        Ok(match domain.object().owner_query(owner) {
            OwnerState::Invalid => hyper::abi::native::HYPER_NATIVE_SYNC_OWNER_INVALID,
            OwnerState::Live => hyper::abi::native::HYPER_NATIVE_SYNC_OWNER_LIVE,
            OwnerState::Dead => hyper::abi::native::HYPER_NATIVE_SYNC_OWNER_DEAD,
        })
    }
    fn sync_domain_wait(
        &self,
        receiver: &InvocationReceiver,
        request: SyncWaitRequest,
    ) -> Result<Outcome, Error> {
        let domain = receiver
            .resolve::<SyncDomain>(Rights::SYNC_WAIT)
            .map_err(ProcessError::from)?
            .into_operation_pin();
        let source = self.process.resolve_invocation(request.source)?;
        let rights = Rights::READ.union(Rights::WRITE);
        let word = if source.kind() == ObjectKind::FILE {
            let file = source
                .resolve::<FileObject>(rights)
                .map_err(ProcessError::from)?;
            sync_wait::Word::prepare_file(
                self.process,
                file.object(),
                domain.koid().get(),
                request.offset,
            )?
        } else {
            let vmo = source
                .resolve::<VmoObject>(rights)
                .map_err(ProcessError::from)?
                .into_operation_pin();
            sync_wait::Word::prepare(self.process, vmo, domain.koid().get(), request.offset)?
        };
        let cancellation = match request.cancellation {
            Some(handle) => {
                let event = self.process.resolve_invocation(handle)?;
                let event = event
                    .resolve::<Event>(Rights::WAIT)
                    .map_err(ProcessError::from)?
                    .into_operation_pin();
                Some(PreparedCancellation::new(
                    event,
                    &self.process.resource_domain(),
                )?)
            }
            None => None,
        };
        sync_wait::wait(self.process, domain, word, request, cancellation, || {
            self.thread.snapshot().phase == UserThreadPhase::StopRequested
        })
    }
    fn sync_domain_wake(
        &self,
        receiver: &InvocationReceiver,
        source: HandleValue,
        offset: u64,
        count: u32,
    ) -> Result<u64, Error> {
        let domain = receiver
            .resolve::<SyncDomain>(Rights::SYNC_WAKE)
            .map_err(ProcessError::from)?;
        let source = self.process.resolve_invocation(source)?;
        let key = if source.kind() == ObjectKind::FILE {
            let file = source
                .resolve::<FileObject>(Rights::WRITE)
                .map_err(ProcessError::from)?;
            let (filesystem, node, absolute) = file
                .object()
                .shared_atomic_key(Some(domain.koid().get()), offset, 8)
                .map_err(file_error)?;
            sync_wait::Key::file(domain.koid().get(), filesystem, node, absolute)
        } else {
            let vmo = source
                .resolve::<VmoObject>(Rights::WRITE)
                .map_err(ProcessError::from)?;
            vmo.object().validate_shared_atomic_u64_offset(offset)?;
            let (backing, absolute) = vmo.object().sync_key(domain.koid().get(), offset)?;
            sync_wait::Key::new(domain.koid().get(), backing, absolute)
        };
        sync_wait::wake(key, count)
    }
}

fn file_error(error: crate::fs::vfs::VfsError) -> Error {
    match error {
        error if error.is_backing_unavailable() => Error::BackingUnavailable,
        other => Error::File(other),
    }
}
