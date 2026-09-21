// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected-architecture host memory mechanisms.
//!
//! Kernel memory policy owns allocation, mapping lifecycle, and protection
//! policy. This facade exposes stage-1 construction and activation plus the
//! architecture implementations of the HAL address, cache, barrier, and
//! atomic-capability contracts. Their architecture-specific semantics remain
//! visible through those HAL traits rather than a weaker common operation.
//! Unsafe operations are direct selected-backend exports, so their
//! architecture-specific safety preconditions remain part of the callable
//! contract. The aliases introduce no runtime dispatch or wrapper state.

pub use super::imp::{
    ActivationContext, ArchitectureAddressTranslation as AddressTranslation,
    ArchitectureBarrier as Barrier, ArchitectureCache as Cache, AtomicCapabilities,
    MemoryError as Error, PreparedAddressSpace, SecondaryActivationContext, StackMapping,
};

pub(crate) use super::imp::{
    activate_memory as activate, atomic_capabilities,
    enable_local_memory_protection as enable_local_protection,
    local_memory_protection_enabled as local_protection_enabled, prepare_address_space as prepare,
};

pub(crate) fn prepare_cache(
    platform: &super::platform::EssentialInfo,
) -> Result<(), hyper::hal::cache::CacheError> {
    super::imp::prepare_cache(platform.as_backend())
}

pub(crate) fn service_stage1_tlb_shootdown() -> bool {
    super::imp::service_stage1_tlb_shootdown()
}

#[cfg(CONFIG_CRASH_CONSOLE)]
pub(crate) use super::imp::inspect_stage1_mapping;

#[cfg(all(CONFIG_ARCH_X86_64, feature = "kernel-self-test"))]
pub(crate) use super::imp::stage1_shootdown_count_for_test;
