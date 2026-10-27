// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native process builder service calls under borrowed Process authority.

use crate::capability::InvocationReceiver;

use super::ProcessServices;
use super::affinity::AffinityInputError;
use crate::mm::user_space::UserSlice;
use crate::object::ObjectKind;
use alloc::vec::Vec;
use {
    crate::abi::native::HierarchyServices, crate::abi::native::ProcessBuilderServiceError,
    crate::abi::native::ProcessBuilderServices,
};
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceKind,
};
use {crate::capability::HandleValue, crate::capability::Rights};
use {
    crate::process::ProcessBuilder, crate::process::ProcessError, crate::process::StartupCapability,
};

struct ChargedBuilderInput {
    bytes: Vec<u8>,
    _charge: Option<CommittedCharge>,
}

impl ChargedBuilderInput {
    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl ProcessServices<'_> {
    fn copy_builder_input(
        &self,
        input: Option<UserSlice>,
    ) -> Result<ChargedBuilderInput, ProcessBuilderServiceError> {
        let length = input.map_or(0, UserSlice::length);
        let length =
            usize::try_from(length).map_err(|_| ProcessBuilderServiceError::InvalidInput)?;
        if length == 0 {
            return Ok(ChargedBuilderInput {
                bytes: Vec::new(),
                _charge: None,
            });
        }
        let charge = self
            .process
            .resource_domain()
            .reserve(ResourceAmount::ZERO.with(
                ResourceKind::KernelMemoryBytes,
                u64::try_from(length).map_err(|_| ProcessBuilderServiceError::InvalidInput)?,
            ))
            .map_err(ProcessError::from)?
            .commit();
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| ProcessBuilderServiceError::Process(ProcessError::Allocation))?;
        bytes.resize(length, 0);
        if let Some(input) = input {
            self.process.copy_from_user(input, &mut bytes)?;
        }
        Ok(ChargedBuilderInput {
            bytes,
            _charge: Some(charge),
        })
    }
}

impl ProcessBuilderServices for ProcessServices<'_> {
    fn create_process_builder(
        &self,
        factory: &InvocationReceiver,
        group: HandleValue,
        domain: HandleValue,
        executable: HandleValue,
        loader: HandleValue,
    ) -> Result<HandleValue, ProcessBuilderServiceError> {
        let current = self
            .thread
            .scheduler_id()
            .ok_or(ProcessBuilderServiceError::InvalidInput)?;
        let scheduling =
            crate::task::scheduler::scheduling_snapshot(current).map_err(ProcessError::from)?;
        crate::process::create_process_builder(
            self.process,
            scheduling,
            factory,
            group,
            domain,
            executable,
            loader,
        )
        .map_err(ProcessBuilderServiceError::Builder)
    }

    fn set_process_builder_name(
        &self,
        builder: &InvocationReceiver,
        name: Option<UserSlice>,
    ) -> Result<(), ProcessBuilderServiceError> {
        let builder = builder
            .resolve::<ProcessBuilder>(Rights::WRITE)
            .map_err(crate::process::ProcessError::from)?;
        let bytes = self.copy_builder_input(name)?;
        let name = core::str::from_utf8(bytes.as_bytes())
            .map_err(|_| ProcessBuilderServiceError::InvalidInput)?;
        builder.object().set_name(name)?;
        Ok(())
    }

    fn set_process_builder_data(
        &self,
        builder: &InvocationReceiver,
        data: Option<UserSlice>,
    ) -> Result<(), ProcessBuilderServiceError> {
        let builder = builder
            .resolve::<ProcessBuilder>(Rights::WRITE)
            .map_err(crate::process::ProcessError::from)?;
        let bytes = self.copy_builder_input(data)?;
        builder.object().set_data(bytes.as_bytes())?;
        Ok(())
    }

    fn set_process_builder_affinity(
        &self,
        builder: &InvocationReceiver,
        words: Option<UserSlice>,
        word_count: usize,
    ) -> Result<(), ProcessBuilderServiceError> {
        let builder = builder
            .resolve::<ProcessBuilder>(Rights::WRITE)
            .map_err(crate::process::ProcessError::from)?;
        let affinity = self
            .copy_affinity(words, word_count)
            .map_err(|error| match error {
                AffinityInputError::Invalid => ProcessBuilderServiceError::InvalidInput,
                AffinityInputError::Memory(error) => ProcessBuilderServiceError::Process(error),
            })?;
        builder.object().set_affinity(affinity)?;
        Ok(())
    }

    fn set_process_builder_memory_lock(
        &self,
        builder: &InvocationReceiver,
        enabled: bool,
    ) -> Result<(), ProcessBuilderServiceError> {
        let builder = builder
            .resolve::<ProcessBuilder>(Rights::WRITE.union(Rights::LOCK_MEMORY))
            .map_err(crate::process::ProcessError::from)?;
        builder.object().set_memory_lock(enabled)?;
        Ok(())
    }

    fn add_process_builder_handle(
        &self,
        builder: &InvocationReceiver,
        source: HandleValue,
        purpose: u32,
        expected_kind: ObjectKind,
        requested_rights: Option<Rights>,
        operation: crate::capability::HandleTransferOperation,
    ) -> Result<(), ProcessBuilderServiceError> {
        let builder = builder
            .resolve::<ProcessBuilder>(Rights::WRITE)
            .map_err(crate::process::ProcessError::from)?;
        builder.object().add_startup_capability(
            self.process,
            StartupCapability::new(
                purpose,
                source,
                requested_rights,
                Some(expected_kind),
                operation,
            ),
        )?;
        Ok(())
    }

    fn seal_process_builder(
        &self,
        builder: &InvocationReceiver,
    ) -> Result<(), ProcessBuilderServiceError> {
        let builder = builder
            .resolve::<ProcessBuilder>(Rights::WRITE)
            .map_err(crate::process::ProcessError::from)?;
        builder.object().seal()?;
        Ok(())
    }

    fn start_process_builder(
        &self,
        builder: &InvocationReceiver,
    ) -> Result<[HandleValue; 2], ProcessBuilderServiceError> {
        let started = crate::process::start_process_builder(self.process, builder)
            .map_err(ProcessBuilderServiceError::Start)?;
        Ok([started.supervisor_handle(), started.startup_channel()])
    }

    fn abort_process_builder(
        &self,
        builder: &InvocationReceiver,
    ) -> Result<(), ProcessBuilderServiceError> {
        crate::process::abort_process_builder(self.process, builder)?;
        Ok(())
    }
}

impl HierarchyServices for ProcessServices<'_> {
    fn create_resource_domain(
        &self,
        parent: &InvocationReceiver,
        limits: crate::accounting::ResourceLimits,
    ) -> Result<HandleValue, crate::process::hierarchy::Error> {
        crate::process::hierarchy::create_resource_domain(self.process, parent, limits)
    }

    fn create_task_group(
        &self,
        factory: &InvocationReceiver,
        domain: HandleValue,
    ) -> Result<HandleValue, crate::process::hierarchy::Error> {
        crate::process::hierarchy::create_task_group(self.process, factory, domain)
    }
}
