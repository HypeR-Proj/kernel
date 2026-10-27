// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Per-process capabilities for operations without another receiver object.
//!
//! Local contexts carry an identity, not a Process owner. Keeping a handle alive
//! cannot retain a dead process, move its quota sponsorship, or create a cycle
//! through its handle table. They cannot be transferred to another process.

use crate::authority::Rights;
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
};
use {crate::capability::HandleFlags, crate::capability::PreparedHandle};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::ObjectPublication,
    crate::object::TransferClass, crate::object::object_allocation_size, crate::object::private,
};

use super::{Process, ProcessError, ProcessId};

/// Authority over the invoking process's execution and wait context.
pub(crate) struct ExecutionContext {
    process_id: ProcessId,
    _charge: CommittedCharge,
}

impl ExecutionContext {
    pub(crate) const fn process_id(&self) -> ProcessId {
        self.process_id
    }
}

impl private::Sealed for ExecutionContext {}
impl private::UserExportable for ExecutionContext {}
impl KernelObject for ExecutionContext {
    const KIND: ObjectKind = ObjectKind::EXECUTION_CONTEXT;
    const TRANSFER_CLASS: TransferClass = TransferClass::Never;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::INSPECT)
        .union(Rights::READ)
        .union(Rights::WRITE)
        .union(Rights::REQUEST_STOP)
        .union(Rights::CREATE_THREAD)
        .union(crate::task::scheduling_authority::SCHEDULING_RIGHTS);
}

/// Constructs local objects charged to the invoking process's resource domain.
pub(crate) struct ObjectFactory {
    process_id: ProcessId,
    _charge: CommittedCharge,
}

impl ObjectFactory {
    pub(crate) const fn process_id(&self) -> ProcessId {
        self.process_id
    }
}

impl private::Sealed for ObjectFactory {}
impl private::UserExportable for ObjectFactory {}
impl KernelObject for ObjectFactory {
    const KIND: ObjectKind = ObjectKind::OBJECT_FACTORY;
    const TRANSFER_CLASS: TransferClass = TransferClass::Never;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::INSPECT)
        .union(Rights::CREATE_OBJECT);
}

/// Read-only ABI, configuration and clock queries; no global task authority.
pub(crate) struct SystemInfo {
    _charge: CommittedCharge,
}

impl private::Sealed for SystemInfo {}
impl private::UserExportable for SystemInfo {}
impl KernelObject for SystemInfo {
    const KIND: ObjectKind = ObjectKind::SYSTEM_INFO;
    const TRANSFER_CLASS: TransferClass = TransferClass::Leaf;
    const SUPPORTED_RIGHTS: Rights = Rights::DUPLICATE
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::READ);
}

fn reserve_object_charge<T: KernelObject>(
    domain: &ResourceDomain,
) -> Result<CommittedCharge, ProcessError> {
    let bytes = object_allocation_size::<T>()
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(ProcessError::Allocation)?;
    Ok(domain
        .reserve(
            ResourceAmount::ZERO
                .with(ResourceKind::KernelObjects, 1)
                .with(ResourceKind::KernelMemoryBytes, bytes),
        )?
        .commit())
}

fn prepare<T: KernelObject + private::UserExportable>(
    object: T,
) -> Result<PreparedHandle, ProcessError> {
    Ok(PreparedHandle::try_from_new_object(
        ObjectPublication::try_new(object)?,
        T::SUPPORTED_RIGHTS,
        HandleFlags::NONE,
    )?)
}

impl Process {
    /// Validates process-local capabilities against the invoking Process. Other
    /// object kinds derive their authority from the handle grant alone.
    pub(crate) fn validate_invocation_context(
        &self,
        receiver: &crate::capability::InvocationReceiver,
    ) -> Result<(), ProcessError> {
        let owner = match receiver.kind() {
            ObjectKind::EXECUTION_CONTEXT => receiver
                .resolve::<ExecutionContext>(Rights::NONE)?
                .object()
                .process_id(),
            ObjectKind::OBJECT_FACTORY => receiver
                .resolve::<ObjectFactory>(Rights::NONE)?
                .object()
                .process_id(),
            _ => return Ok(()),
        };
        if owner == self.id() {
            Ok(())
        } else {
            Err(crate::capability::HandleError::AccessDenied.into())
        }
    }

    /// Prepares the loader's execution, object factory and system information
    /// handles in that order. Publication is part of the caller's startup
    /// transaction; partial allocation releases every charge automatically.
    pub(crate) fn prepare_invocation_handles(&self) -> Result<[PreparedHandle; 3], ProcessError> {
        let domain = self.resource_domain();
        Ok([
            PreparedHandle::try_from_new_object(
                ObjectPublication::try_new(ExecutionContext {
                    process_id: self.id(),
                    _charge: reserve_object_charge::<ExecutionContext>(&domain)?,
                })?,
                Rights::DUPLICATE
                    .union(Rights::INSPECT)
                    .union(Rights::READ)
                    .union(Rights::WRITE)
                    .union(Rights::REQUEST_STOP)
                    .union(Rights::CREATE_THREAD)
                    .union(self.scheduling().delegated),
                HandleFlags::NONE,
            )?,
            prepare(ObjectFactory {
                process_id: self.id(),
                _charge: reserve_object_charge::<ObjectFactory>(&domain)?,
            })?,
            prepare(SystemInfo {
                _charge: reserve_object_charge::<SystemInfo>(&domain)?,
            })?,
        ])
    }

    /// Test images bypass the userspace loader but receive the same capabilities.
    #[cfg(all(
        feature = "kernel-test",
        feature = "kernel-self-test",
        any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)
    ))]
    pub(crate) fn publish_invocation_handles(&self) -> Result<InvocationHandles, ProcessError> {
        let handles = self.prepare_invocation_handles()?;
        let reservation = self.reserve_handles::<3>()?;
        let [execution_context, object_factory, system_info] = self
            .publish_handles(reservation, handles)
            .map_err(|failure| failure.error)?;
        Ok(InvocationHandles {
            execution_context,
            object_factory,
            system_info,
        })
    }
}

#[cfg(all(
    feature = "kernel-test",
    feature = "kernel-self-test",
    any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)
))]
pub(crate) struct InvocationHandles {
    pub(crate) execution_context: crate::capability::HandleValue,
    pub(crate) object_factory: crate::capability::HandleValue,
    pub(crate) system_info: crate::capability::HandleValue,
}

#[cfg(all(
    feature = "kernel-test",
    feature = "kernel-self-test",
    any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)
))]
impl Process {
    /// Publishes the real context objects, then checks process binding and all
    /// transfer routes without granting a foreign process their capabilities.
    pub(crate) fn verify_invocation_context_for_test(
        &self,
        foreign: &Process,
    ) -> Result<(), &'static str> {
        use {
            crate::capability::HandleError, crate::capability::HandleTransferOperation,
            crate::capability::HandleTransferRequest, crate::capability::HandleTransferRoute,
        };
        if self.id() == foreign.id() {
            return Err("distinct context owners");
        }
        let handles = self
            .publish_invocation_handles()
            .map_err(|_| "context publication")?;
        let values = [
            handles.execution_context,
            handles.object_factory,
            handles.system_info,
        ];
        let result = (|| {
            for (index, handle) in values.into_iter().enumerate() {
                let receiver = self
                    .resolve_invocation(handle)
                    .map_err(|_| "context receiver")?;
                self.validate_invocation_context(&receiver)
                    .map_err(|_| "owner admission")?;
                let rights = receiver.info().rights;
                if index == 2 {
                    foreign
                        .validate_invocation_context(&receiver)
                        .map_err(|_| "system info delegation")?;
                    if !rights.contains(Rights::READ.union(Rights::TRANSFER)) {
                        return Err("system info rights");
                    }
                    let request = HandleTransferRequest {
                        value: handle,
                        offered_rights: None,
                        rights: Rights::READ,
                        offered_kind: None,
                        expected_kind: None,
                        operation: HandleTransferOperation::Move,
                    };
                    self.prepare_handle_transfer(
                        &[request],
                        None,
                        None,
                        HandleTransferRoute::Buffered,
                    )
                    .map_err(|_| "system info transfer")?
                    .rollback();
                    continue;
                }
                if !matches!(
                    foreign.validate_invocation_context(&receiver),
                    Err(ProcessError::Handle(HandleError::AccessDenied))
                ) {
                    return Err("foreign context admitted");
                }
                if rights.contains(Rights::TRANSFER) || rights.contains(Rights::WAIT) {
                    return Err("context grants propagation or signals");
                }
                let reduced = self
                    .duplicate_handle(handle, Rights::INSPECT)
                    .map_err(|_| "context attenuation")?;
                let reduced_result = (|| {
                    let reduced = self
                        .resolve_invocation(reduced)
                        .map_err(|_| "attenuated context receiver")?;
                    self.validate_invocation_context(&reduced)
                        .map_err(|_| "attenuated owner admission")?;
                    let operation_right = if index == 0 {
                        Rights::WRITE
                    } else {
                        Rights::CREATE_OBJECT
                    };
                    if reduced.require(operation_right) != Err(HandleError::AccessDenied) {
                        return Err("attenuation granted operation");
                    }
                    Ok(())
                })();
                self.close_handle(reduced)
                    .map_err(|_| "close attenuated context")?;
                reduced_result?;
                for route in [
                    HandleTransferRoute::Buffered,
                    HandleTransferRoute::Rendezvous,
                    HandleTransferRoute::StagedStartup,
                ] {
                    let request = HandleTransferRequest {
                        value: handle,
                        offered_rights: None,
                        rights: Rights::INSPECT,
                        offered_kind: None,
                        expected_kind: None,
                        operation: HandleTransferOperation::Move,
                    };
                    match self.prepare_handle_transfer(&[request], None, None, route) {
                        Err(ProcessError::Handle(HandleError::AccessDenied)) => (),
                        Ok(claim) => {
                            claim.rollback();
                            return Err("context transferred");
                        }
                        Err(_) => return Err("unexpected context transfer failure"),
                    }
                }
            }
            Ok(())
        })();
        let mut closed = Ok(());
        for value in values {
            if self.close_handle(value).is_err() {
                closed = Err("close invocation context");
            }
        }
        result.and(closed)
    }
}
