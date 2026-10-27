// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Read-only postmortem access under an explicitly delegated capability.

use crate::capability::InvocationReceiver;

use super::ProcessServices;
use crate::authority::Rights;
use {crate::abi::native::ProcessBuilderServiceError, crate::abi::native::RetentionServices};
use {crate::accounting::ResourceAmount, crate::accounting::ResourceKind};
use {crate::capability::HandleSnapshot, crate::capability::HandleValue};
use {crate::mm::user_space::CrashMapping, crate::mm::user_space::UserSlice};
use {crate::process::ProcessBuilder, crate::process::ProcessError};
use {
    crate::process::retention::CrashView, crate::process::retention::Error,
    crate::process::retention::Info, crate::process::retention::ProcessRetirement,
    crate::process::retention::ThreadEvidence,
};

impl RetentionServices for ProcessServices<'_> {
    fn retain_process_builder(
        &self,
        builder: &InvocationReceiver,
        bytes: u64,
        timeout: u64,
    ) -> Result<HandleValue, ProcessBuilderServiceError> {
        let builder = builder
            .resolve::<ProcessBuilder>(Rights::WRITE.union(Rights::RETAIN_PROCESS))
            .map_err(crate::process::ProcessError::from)?;
        // Object allocation and handle capacity are prepared before changing
        // policy. Publishing the prepared handle can now fail only if the
        // caller has stopped; withdrawing its authority revokes retention.
        let reservation = self.process.reserve_handles::<1>()?;
        let control = match builder
            .object()
            .retain_on_exit(self.process, bytes, timeout)
        {
            Ok(control) => control,
            Err(error) => {
                self.process.abort_handles(reservation);
                return Err(error.into());
            }
        };
        self.process
            .publish_handles(reservation, [control])
            .map(|values| values[0])
            .map_err(|failure| failure.error.into())
    }
    fn retention_info(&self, handle: &InvocationReceiver, view: bool) -> Result<Info, Error> {
        if view {
            handle
                .resolve::<CrashView>(Rights::INSPECT)
                .map_err(crate::process::ProcessError::from)?
                .object()
                .info()
        } else {
            Ok(handle
                .resolve::<ProcessRetirement>(Rights::INSPECT)
                .map_err(crate::process::ProcessError::from)?
                .object()
                .info())
        }
    }
    fn retention_open_view(&self, handle: &InvocationReceiver) -> Result<HandleValue, Error> {
        let control = handle
            .resolve::<ProcessRetirement>(Rights::INSPECT_DETAILS)
            .map_err(crate::process::ProcessError::from)?;
        if self.process.is_retention_target(control.object()) {
            return Err(Error::Process(ProcessError::Handle(
                crate::capability::HandleError::AccessDenied,
            )));
        }
        let view = control
            .object()
            .open_view(&self.process.resource_domain())?;
        self.process
            .create_object(view, CrashView::RIGHTS)
            .map_err(Into::into)
    }
    fn retention_release(&self, handle: &InvocationReceiver) -> Result<(), Error> {
        handle
            .resolve::<ProcessRetirement>(Rights::RETIRE)
            .map_err(crate::process::ProcessError::from)?
            .object()
            .release();
        Ok(())
    }
    fn crash_thread(
        &self,
        handle: &InvocationReceiver,
        index: usize,
    ) -> Result<(ThreadEvidence, usize), Error> {
        handle
            .resolve::<CrashView>(Rights::READ)
            .map_err(crate::process::ProcessError::from)?
            .object()
            .thread(index)
    }
    fn crash_mapping(
        &self,
        handle: &InvocationReceiver,
        index: usize,
    ) -> Result<(CrashMapping, usize), Error> {
        handle
            .resolve::<CrashView>(Rights::READ)
            .map_err(crate::process::ProcessError::from)?
            .object()
            .mapping(index)
    }
    fn crash_handle(
        &self,
        handle: &InvocationReceiver,
        index: usize,
    ) -> Result<(HandleSnapshot, usize), Error> {
        handle
            .resolve::<CrashView>(Rights::READ)
            .map_err(crate::process::ProcessError::from)?
            .object()
            .handle(index)
    }
    fn crash_read(
        &self,
        handle: &InvocationReceiver,
        address: u64,
        output: UserSlice,
    ) -> Result<usize, Error> {
        let size = usize::try_from(output.length()).map_err(|_| Error::Invalid)?;
        if size == 0 || size > 65536 {
            return Err(Error::Invalid);
        }
        let view = handle
            .resolve::<CrashView>(Rights::READ)
            .map_err(crate::process::ProcessError::from)?;
        let _charge = self
            .process
            .resource_domain()
            .reserve(ResourceAmount::ZERO.with(ResourceKind::KernelMemoryBytes, size as u64))
            .map_err(ProcessError::from)?
            .commit();
        let mut buffer = alloc::vec::Vec::new();
        buffer
            .try_reserve_exact(size)
            .map_err(|_| Error::Process(ProcessError::Allocation))?;
        buffer.resize(size, 0);
        let count = view.object().read(address, &mut buffer)?;
        let target = UserSlice::new(output.base(), count as u64).map_err(|_| Error::Invalid)?;
        self.process.copy_to_user(target, &buffer[..count])?;
        Ok(count)
    }
}
