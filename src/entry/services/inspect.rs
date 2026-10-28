// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native inspection service calls under borrowed Process authority.

use crate::capability::InvocationReceiver;

use super::ProcessServices;
use crate::accounting::ResourceDomainObject;
use crate::object::KernelObject;
use {crate::abi::native::InspectServices, crate::abi::native::SystemInspectServices};
use {crate::capability::HandleValue, crate::capability::Rights};
use {
    crate::diagnostics::inspect::CpuInspector, crate::diagnostics::inspect::MemoryInspector,
    crate::diagnostics::inspect::ObjectInspector, crate::diagnostics::inspect::TaskInspector,
};
use {
    crate::process::ProcessObject, crate::process::ProcessSnapshot, crate::process::TaskGroupObject,
};

impl SystemInspectServices for ProcessServices<'_> {
    fn profile_start(
        &self,
        receiver: &InvocationReceiver,
        period: u64,
        seconds: u64,
    ) -> crate::diagnostics::profile::Result<u64> {
        receiver
            .resolve::<CpuInspector>(Rights::SET_ATTRIBUTES)
            .map_err(|_| hyper::abi::native::HYPER_NATIVE_STATUS_ACCESS_DENIED)?;
        crate::diagnostics::profile::start(period, seconds)
    }
    fn profile_stop(
        &self,
        receiver: &InvocationReceiver,
        generation: u64,
    ) -> crate::diagnostics::profile::Result<()> {
        receiver
            .resolve::<CpuInspector>(Rights::SET_ATTRIBUTES)
            .map_err(|_| hyper::abi::native::HYPER_NATIVE_STATUS_ACCESS_DENIED)?;
        crate::diagnostics::profile::stop(generation)
    }
    fn profile_read(
        &self,
        receiver: &InvocationReceiver,
        generation: u64,
        cpu: u64,
        cursor: u64,
        output: &mut [u8],
    ) -> crate::diagnostics::profile::Result<usize> {
        receiver
            .resolve::<CpuInspector>(Rights::INSPECT_DETAILS)
            .map_err(|_| hyper::abi::native::HYPER_NATIVE_STATUS_ACCESS_DENIED)?;
        crate::diagnostics::profile::query(generation, cpu, cursor, output)
    }

    fn trace_start(
        &self,
        receiver: &InvocationReceiver,
        config: &[u8; 56],
    ) -> crate::diagnostics::trace::Result<u64> {
        receiver
            .resolve::<CpuInspector>(Rights::SET_ATTRIBUTES)
            .map_err(|_| hyper::abi::native::HYPER_NATIVE_STATUS_ACCESS_DENIED)?;
        crate::diagnostics::trace::capture_start(config)
    }
    fn trace_stop(
        &self,
        receiver: &InvocationReceiver,
        generation: u64,
    ) -> crate::diagnostics::trace::Result<()> {
        receiver
            .resolve::<CpuInspector>(Rights::SET_ATTRIBUTES)
            .map_err(|_| hyper::abi::native::HYPER_NATIVE_STATUS_ACCESS_DENIED)?;
        crate::diagnostics::trace::capture_stop(generation)
    }
    fn trace_clear(
        &self,
        receiver: &InvocationReceiver,
        generation: u64,
    ) -> crate::diagnostics::trace::Result<()> {
        receiver
            .resolve::<CpuInspector>(Rights::SET_ATTRIBUTES)
            .map_err(|_| hyper::abi::native::HYPER_NATIVE_STATUS_ACCESS_DENIED)?;
        crate::diagnostics::trace::capture_clear(generation)
    }
    fn trace_read(
        &self,
        receiver: &InvocationReceiver,
        generation: u64,
        cpu: u64,
        cursor: u64,
        output: &mut [u8],
    ) -> crate::diagnostics::trace::Result<usize> {
        receiver
            .resolve::<CpuInspector>(Rights::INSPECT_DETAILS)
            .map_err(|_| hyper::abi::native::HYPER_NATIVE_STATUS_ACCESS_DENIED)?;
        crate::diagnostics::trace::query(generation, cpu, cursor, output)
    }

    fn memory_statistics(
        &self,
        inspector: &InvocationReceiver,
        query: crate::diagnostics::inspect::memory::Query,
    ) -> Result<crate::diagnostics::inspect::memory::Statistics, crate::diagnostics::inspect::Error>
    {
        let inspector = inspector
            .resolve::<MemoryInspector>(Rights::INSPECT)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        inspector.object().statistics(query)
    }
    fn memory_observation(
        &self,
        inspector: &InvocationReceiver,
    ) -> Result<crate::diagnostics::inspect::MemoryObservation, crate::diagnostics::inspect::Error>
    {
        let inspector = inspector
            .resolve::<MemoryInspector>(Rights::INSPECT)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        inspector.object().snapshot()
    }

    fn cpu_observation(
        &self,
        inspector: &InvocationReceiver,
    ) -> Result<crate::task::scheduler::CpuTimeSnapshot, crate::diagnostics::inspect::Error> {
        let inspector = inspector
            .resolve::<CpuInspector>(Rights::INSPECT)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        Ok(inspector.object().snapshot())
    }
}

impl InspectServices for ProcessServices<'_> {
    fn scan_processes(
        &self,
        inspector: &InvocationReceiver,
        cursor: u64,
        output: &mut crate::diagnostics::inspect::Page<
            ProcessSnapshot,
            { crate::diagnostics::inspect::PROCESS_PAGE_CAPACITY },
        >,
    ) -> Result<(), crate::diagnostics::inspect::Error> {
        let inspector = inspector
            .resolve::<TaskInspector>(Rights::INSPECT)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        inspector.object().scan_processes(cursor, output)
    }

    fn scan_threads(
        &self,
        inspector: &InvocationReceiver,
        cursor: u64,
        output: &mut crate::diagnostics::inspect::Page<
            crate::diagnostics::inspect::TaskThreadSnapshot,
            { crate::diagnostics::inspect::THREAD_PAGE_CAPACITY },
        >,
    ) -> Result<(), crate::diagnostics::inspect::Error> {
        let inspector = inspector
            .resolve::<TaskInspector>(Rights::INSPECT)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        inspector.object().scan_threads(cursor, output)
    }

    fn read_object_details(
        &self,
        inspector: &InvocationReceiver,
        process: u64,
        target: u64,
        cursor: u64,
    ) -> Result<crate::object::diagnostics::ObjectDetails, crate::diagnostics::inspect::Error> {
        let inspector = inspector
            .resolve::<ObjectInspector>(Rights::INSPECT.union(Rights::INSPECT_DETAILS))
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        inspector.object().read_details(process, target, cursor)
    }

    fn scan_objects(
        &self,
        inspector: &InvocationReceiver,
        cursor: u64,
        output: &mut crate::diagnostics::inspect::Page<
            crate::object::ObjectSnapshot,
            { crate::diagnostics::inspect::OBJECT_PAGE_CAPACITY },
        >,
    ) -> Result<(), crate::diagnostics::inspect::Error> {
        let inspector = inspector
            .resolve::<ObjectInspector>(Rights::INSPECT)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        inspector.object().scan_objects(cursor, output)
    }

    fn scan_process_handles(
        &self,
        inspector: &InvocationReceiver,
        process_koid: u64,
        cursor: u64,
    ) -> Result<
        crate::diagnostics::inspect::Page<
            crate::diagnostics::inspect::ProcessHandleSnapshot,
            { crate::diagnostics::inspect::HANDLE_PAGE_CAPACITY },
        >,
        crate::diagnostics::inspect::Error,
    > {
        let inspector = inspector
            .resolve::<ObjectInspector>(Rights::INSPECT)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        inspector
            .object()
            .scan_process_handles(process_koid, cursor)
    }

    fn derive_task_inspector(
        &self,
        inspector: &InvocationReceiver,
        process: HandleValue,
    ) -> Result<HandleValue, crate::diagnostics::inspect::Error> {
        let inspector = inspector
            .resolve::<TaskInspector>(<TaskInspector as KernelObject>::SUPPORTED_RIGHTS)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        let target = self
            .process
            .resolve_handle::<ProcessObject>(process, Rights::INSPECT)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        let derived = inspector
            .object()
            .try_derive_process(target.object(), &self.process.resource_domain())?;
        self.process
            .create_object(derived, <TaskInspector as KernelObject>::SUPPORTED_RIGHTS)
            .map_err(crate::diagnostics::inspect::Error::Process)
    }

    fn derive_object_inspector(
        &self,
        inspector: &InvocationReceiver,
        process: HandleValue,
    ) -> Result<HandleValue, crate::diagnostics::inspect::Error> {
        let inspector = inspector
            .resolve::<ObjectInspector>(ObjectInspector::DERIVATION_RIGHTS)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        let target = self
            .process
            .resolve_handle::<ProcessObject>(process, Rights::INSPECT)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        let derived = inspector
            .object()
            .try_derive_process(target.object(), &self.process.resource_domain())?;
        self.process
            .create_object(derived, ObjectInspector::DERIVATION_RIGHTS)
            .map_err(crate::diagnostics::inspect::Error::Process)
    }

    fn derive_task_inspector_for_task_group(
        &self,
        inspector: &InvocationReceiver,
        group: HandleValue,
    ) -> Result<HandleValue, crate::diagnostics::inspect::Error> {
        let inspector = inspector
            .resolve::<TaskInspector>(<TaskInspector as KernelObject>::SUPPORTED_RIGHTS)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        let target = self
            .process
            .resolve_handle::<TaskGroupObject>(group, Rights::INSPECT)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        let derived = inspector
            .object()
            .try_derive_task_group(target.object(), &self.process.resource_domain())?;
        self.process
            .create_object(derived, <TaskInspector as KernelObject>::SUPPORTED_RIGHTS)
            .map_err(crate::diagnostics::inspect::Error::Process)
    }

    fn derive_object_inspector_for_task_group(
        &self,
        inspector: &InvocationReceiver,
        group: HandleValue,
    ) -> Result<HandleValue, crate::diagnostics::inspect::Error> {
        let inspector = inspector
            .resolve::<ObjectInspector>(ObjectInspector::DERIVATION_RIGHTS)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        let target = self
            .process
            .resolve_handle::<TaskGroupObject>(group, Rights::INSPECT)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        let derived = inspector
            .object()
            .try_derive_task_group(target.object(), &self.process.resource_domain())?;
        self.process
            .create_object(derived, ObjectInspector::DERIVATION_RIGHTS)
            .map_err(crate::diagnostics::inspect::Error::Process)
    }

    fn derive_task_inspector_for_resource_domain(
        &self,
        inspector: &InvocationReceiver,
        domain: HandleValue,
    ) -> Result<HandleValue, crate::diagnostics::inspect::Error> {
        let inspector = inspector
            .resolve::<TaskInspector>(<TaskInspector as KernelObject>::SUPPORTED_RIGHTS)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        let target = self
            .process
            .resolve_handle::<ResourceDomainObject>(domain, Rights::INSPECT)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        let derived = inspector
            .object()
            .try_derive_resource_domain(target.object(), &self.process.resource_domain())?;
        self.process
            .create_object(derived, <TaskInspector as KernelObject>::SUPPORTED_RIGHTS)
            .map_err(crate::diagnostics::inspect::Error::Process)
    }

    fn derive_object_inspector_for_resource_domain(
        &self,
        inspector: &InvocationReceiver,
        domain: HandleValue,
    ) -> Result<HandleValue, crate::diagnostics::inspect::Error> {
        let inspector = inspector
            .resolve::<ObjectInspector>(ObjectInspector::DERIVATION_RIGHTS)
            .map_err(crate::process::ProcessError::from)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        let target = self
            .process
            .resolve_handle::<ResourceDomainObject>(domain, Rights::INSPECT)
            .map_err(crate::diagnostics::inspect::Error::Process)?;
        let derived = inspector
            .object()
            .try_derive_resource_domain(target.object(), &self.process.resource_domain())?;
        self.process
            .create_object(derived, ObjectInspector::DERIVATION_RIGHTS)
            .map_err(crate::diagnostics::inspect::Error::Process)
    }
}
