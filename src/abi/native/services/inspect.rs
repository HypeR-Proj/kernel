// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Read-only system and object observation contracts.

use super::memory::UserMemoryServices;
use crate::capability::{HandleValue, InvocationReceiver};
use crate::diagnostics::inspect::{HANDLE_PAGE_CAPACITY, OBJECT_PAGE_CAPACITY, Page};
use crate::process::ProcessSnapshot;

pub(crate) trait SystemInspectServices: UserMemoryServices {
    fn profile_start(
        &self,
        _receiver: &InvocationReceiver,
        _period: u64,
        _seconds: u64,
    ) -> crate::diagnostics::profile::Result<u64> {
        Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
    }
    fn profile_stop(
        &self,
        _receiver: &InvocationReceiver,
        _generation: u64,
    ) -> crate::diagnostics::profile::Result<()> {
        Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
    }
    fn profile_read(
        &self,
        _receiver: &InvocationReceiver,
        _generation: u64,
        _cpu: u64,
        _cursor: u64,
        _output: &mut [u8],
    ) -> crate::diagnostics::profile::Result<usize> {
        Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
    }

    fn trace_start(
        &self,
        _receiver: &InvocationReceiver,
        _config: &[u8; 56],
    ) -> crate::diagnostics::trace::Result<u64> {
        Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
    }
    fn trace_stop(
        &self,
        _receiver: &InvocationReceiver,
        _generation: u64,
    ) -> crate::diagnostics::trace::Result<()> {
        Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
    }
    fn trace_clear(
        &self,
        _receiver: &InvocationReceiver,
        _generation: u64,
    ) -> crate::diagnostics::trace::Result<()> {
        Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
    }
    fn trace_read(
        &self,
        _receiver: &InvocationReceiver,
        _generation: u64,
        _cpu: u64,
        _cursor: u64,
        _output: &mut [u8],
    ) -> crate::diagnostics::trace::Result<usize> {
        Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
    }

    fn memory_statistics(
        &self,
        inspector: &InvocationReceiver,
        query: crate::diagnostics::inspect::memory::Query,
    ) -> Result<crate::diagnostics::inspect::memory::Statistics, crate::diagnostics::inspect::Error>;
    fn memory_observation(
        &self,
        inspector: &InvocationReceiver,
    ) -> Result<crate::diagnostics::inspect::MemoryObservation, crate::diagnostics::inspect::Error>;
    fn cpu_observation(
        &self,
        inspector: &InvocationReceiver,
    ) -> Result<crate::task::scheduler::CpuTimeSnapshot, crate::diagnostics::inspect::Error>;
}

pub(crate) trait InspectServices: UserMemoryServices {
    fn read_object_details(
        &self,
        inspector: &InvocationReceiver,
        process: u64,
        target: u64,
        cursor: u64,
    ) -> Result<crate::object::diagnostics::ObjectDetails, crate::diagnostics::inspect::Error>;

    fn scan_processes(
        &self,
        inspector: &InvocationReceiver,
        cursor: u64,
        output: &mut Page<ProcessSnapshot, { crate::diagnostics::inspect::PROCESS_PAGE_CAPACITY }>,
    ) -> Result<(), crate::diagnostics::inspect::Error>;
    /// Fills caller-owned snapshot storage. Contents are publishable only on success.
    fn scan_threads(
        &self,
        inspector: &InvocationReceiver,
        cursor: u64,
        output: &mut Page<
            crate::diagnostics::inspect::TaskThreadSnapshot,
            { crate::diagnostics::inspect::THREAD_PAGE_CAPACITY },
        >,
    ) -> Result<(), crate::diagnostics::inspect::Error>;
    fn scan_objects(
        &self,
        inspector: &InvocationReceiver,
        cursor: u64,
        output: &mut Page<crate::object::ObjectSnapshot, OBJECT_PAGE_CAPACITY>,
    ) -> Result<(), crate::diagnostics::inspect::Error>;
    fn scan_process_handles(
        &self,
        inspector: &InvocationReceiver,
        process_koid: u64,
        cursor: u64,
    ) -> Result<
        Page<crate::diagnostics::inspect::ProcessHandleSnapshot, HANDLE_PAGE_CAPACITY>,
        crate::diagnostics::inspect::Error,
    >;
    fn derive_task_inspector(
        &self,
        inspector: &InvocationReceiver,
        process: HandleValue,
    ) -> Result<HandleValue, crate::diagnostics::inspect::Error>;
    fn derive_object_inspector(
        &self,
        inspector: &InvocationReceiver,
        process: HandleValue,
    ) -> Result<HandleValue, crate::diagnostics::inspect::Error>;
    fn derive_task_inspector_for_task_group(
        &self,
        inspector: &InvocationReceiver,
        group: HandleValue,
    ) -> Result<HandleValue, crate::diagnostics::inspect::Error>;
    fn derive_object_inspector_for_task_group(
        &self,
        inspector: &InvocationReceiver,
        group: HandleValue,
    ) -> Result<HandleValue, crate::diagnostics::inspect::Error>;
    fn derive_task_inspector_for_resource_domain(
        &self,
        inspector: &InvocationReceiver,
        domain: HandleValue,
    ) -> Result<HandleValue, crate::diagnostics::inspect::Error>;
    fn derive_object_inspector_for_resource_domain(
        &self,
        inspector: &InvocationReceiver,
        domain: HandleValue,
    ) -> Result<HandleValue, crate::diagnostics::inspect::Error>;
}
