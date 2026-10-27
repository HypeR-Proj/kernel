// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded, read-only allocator diagnostics under `MemoryInspector` authority.
use crate::capability::InvocationReceiver;

use crate::diagnostics::inspect::memory::Query;
use hyper::abi::native::*;
use {
    crate::abi::native::Arguments, crate::abi::native::services::NativeAction,
    crate::abi::native::services::SystemInspectServices, crate::abi::native::status::info_result,
    crate::abi::native::status::status_from_inspection_error,
    crate::abi::native::wire::copy_info_record, crate::abi::native::wire::memory_statistics,
    crate::abi::native::wire::prepare_info_request,
};

const _: () = assert!(
    HYPER_NATIVE_MEMORY_STATISTICS_HEAP_CLASS_COUNT as usize
        == crate::mm::allocator::heap::profile::BUCKETS
);
const _: () = assert!(
    HYPER_NATIVE_MEMORY_STATISTICS_BUDDY_ORDER_COUNT as usize
        == crate::mm::allocator::MAX_ORDER + 1
);

#[inline(never)]
pub(in crate::abi::native) fn sys_memory_inspector_read_statistics(
    services: &impl SystemInspectServices,
    receiver: &InvocationReceiver,
    arguments: &Arguments,
) -> NativeAction {
    let result = (|| {
        if arguments[4] != 0 {
            return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
        }
        let index =
            usize::try_from(arguments[1]).map_err(|_| HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        let (query, size) = match (arguments[0], index) {
            (HYPER_NATIVE_MEMORY_STATISTICS_HEAP, 0) => (
                Query::Heap,
                core::mem::size_of::<HyperNativeMemoryHeapStatistics>(),
            ),
            (HYPER_NATIVE_MEMORY_STATISTICS_BUDDY, index)
                if index < HYPER_NATIVE_MEMORY_STATISTICS_BUDDY_ORDER_COUNT as usize =>
            {
                (
                    Query::Buddy(index),
                    core::mem::size_of::<HyperNativeMemoryBuddyStatistics>(),
                )
            }
            (HYPER_NATIVE_MEMORY_STATISTICS_OWNER, index)
                if index < HYPER_NATIVE_MEMORY_STATISTICS_OWNER_COUNT as usize =>
            {
                (
                    Query::Owner(index),
                    core::mem::size_of::<HyperNativeMemoryOwnerStatistics>(),
                )
            }
            (HYPER_NATIVE_MEMORY_STATISTICS_FILE_CACHE, 0) => (
                Query::FileCache,
                core::mem::size_of::<HyperNativeMemoryFileCacheStatistics>(),
            ),
            (HYPER_NATIVE_MEMORY_STATISTICS_HEAP_CLASS, index)
                if index < HYPER_NATIVE_MEMORY_STATISTICS_HEAP_CLASS_COUNT as usize =>
            {
                (
                    Query::HeapClass(index),
                    core::mem::size_of::<HyperNativeMemoryHeapClassStatistics>(),
                )
            }
            _ => return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT),
        };
        let request = prepare_info_request(&[arguments[2], arguments[3], 0, 0, 0, 0], size, size)?;
        let snapshot = services
            .memory_statistics(receiver, query)
            .map_err(status_from_inspection_error)?;
        let mut record = [0; memory_statistics::MAX_RECORD];
        let encoded = memory_statistics::encode(snapshot, &mut record);
        copy_info_record(services, request, &record[..encoded])
    })();
    NativeAction::Return(info_result(result))
}
