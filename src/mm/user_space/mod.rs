// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-independent ownership for native user virtual memory.
//!
//! The mechanism is generic over page allocation and resource accounting so
//! its transaction and protection rules can be tested on a host. Architecture
//! page tables, translation identifiers, and invalidation live below HAL.

#![cfg_attr(test, allow(unexpected_cfgs))]

mod address_space;
#[cfg(test)]
pub(crate) use address_space::{VmarDetailError, VmarDetailRecord};
#[cfg(not(test))]
mod authority;
mod contract;
mod free_ranges;
mod index;
mod inline_buffer;
#[cfg(not(test))]
mod kernel_adapter;
#[cfg(not(test))]
mod machine;
#[cfg(not(test))]
mod objects;
#[cfg(not(test))]
mod service;
mod transaction;
mod vmo;
#[cfg(test)]
pub(crate) use transaction::retry_stale;
#[cfg(not(test))]
pub(crate) use vmo::{ExclusiveHardwareWriteLease, WritableMappingLease};

pub(crate) use address_space::{
    AddressSpaceError, CrashMapping, CrashMappingKind, CrashMemory, CrashReadError, MappingChange,
    MappingToken, PreparedMappingChange, PreparedUserRead, PreparedUserWrite, UserAddressSpace,
    Vmar,
};
#[cfg(not(test))]
pub(crate) use authority::ExecutableAuthority;
#[cfg(all(not(test), all(feature = "kernel-test", feature = "kernel-self-test")))]
pub(crate) use authority::ExecutableAuthorityError;
pub(crate) use contract::{
    Access, AddressError, MemoryAccount, MemoryCharge, Permissions, UserAddress, UserSlice,
};
#[cfg(not(test))]
pub(crate) use kernel_adapter::address_window;
#[cfg(all(not(test), all(feature = "kernel-test", feature = "kernel-self-test")))]
#[allow(
    unused_imports,
    reason = "shared self-test entry; hardware coverage is architecture-dependent"
)]
pub(crate) use kernel_adapter::fail_exposed_write_after_copy_for_test;
#[cfg(not(test))]
pub(crate) use kernel_adapter::{DomainAccount, KernelPageBackend, KernelPageError};
#[cfg(all(not(test), all(feature = "kernel-test", feature = "kernel-self-test")))]
#[allow(
    unused_imports,
    reason = "shared self-test entry; hardware coverage is architecture-dependent"
)]
pub(crate) use machine::prepare_native_entry_self_test;
#[cfg(all(not(test), all(feature = "kernel-test", feature = "kernel-self-test")))]
#[allow(
    unused_imports,
    reason = "shared self-test entry; hardware coverage is architecture-dependent"
)]
pub(crate) use machine::run_dormant_self_test;
#[cfg(not(test))]
pub(crate) use machine::{
    ActiveNativeAddressSpace, Error as MachineError, NativeAddressSpace, NativeCrashMemory,
    NativeImageSegment, NativePinnedAtomicWord, NativeUserRead, StoppedNativeRun,
    UserWriteReservation,
};
#[cfg(not(test))]
pub(crate) use objects::{GuestMemoryBacking, MemoryObjectError, VmarObject, VmoObject};
#[cfg(not(test))]
pub(crate) use service::{ServiceError as MemoryServiceError, permissions as abi_permissions};
#[cfg(not(test))]
pub(crate) use service::{
    allocate_vmar, create_contiguous_vmo, create_vmo, destroy as destroy_vmar, map_vmo, protect,
    read_vmo, set_memory_residency, unmap, write_vmo,
};

#[cfg(not(test))]
pub(crate) use machine::service_local_rpc;
pub(crate) use vmo::{
    ExecutableProvenance, ExecutableVmo, PrivateMappingMode, SnapshotVmo, VmoError, WritableVmo,
};

#[cfg(not(test))]
pub(crate) use service::{PrivateMappingRequest, create_vmo_snapshot, map_file, map_private};

#[cfg(test)]
pub(crate) use address_space::CommittedMappingChange;
#[cfg(test)]
pub(crate) use contract::{PageBackend, UserAddressWindow};

#[cfg(test)]
pub(crate) use vmo::FilePageSource;
