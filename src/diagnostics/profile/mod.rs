// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded system-wide PMU capture. Buffers exist only in profiling builds.
//! Control/read calls serialize with each other; IRQ producers never wait for them.

#[cfg(all(CONFIG_ARCH_AARCH64, feature = "pmu-profiling"))]
mod admission;
#[cfg(all(CONFIG_ARCH_AARCH64, feature = "pmu-profiling"))]
mod runtime;
#[cfg(all(CONFIG_ARCH_AARCH64, feature = "pmu-profiling"))]
pub(crate) use runtime::{
    initialize, initialize_local_cpu, owns_node, query, service_control, start, stop,
};

#[cfg(all(CONFIG_ARCH_AARCH64, feature = "pmu-profiling"))]
pub(crate) const DEPTH: usize = 16;
pub(crate) const INFO_SIZE: usize = 144;
pub(crate) const SAMPLE_SIZE: usize = 192;
pub(crate) type Result<T> = core::result::Result<T, hyper::abi::native::HyperNativeStatus>;

#[cfg(not(all(CONFIG_ARCH_AARCH64, feature = "pmu-profiling")))]
pub(crate) fn start(_: u64, _: u64) -> Result<u64> {
    Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
}
#[cfg(not(all(CONFIG_ARCH_AARCH64, feature = "pmu-profiling")))]
pub(crate) fn stop(_: u64) -> Result<()> {
    Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
}
#[cfg(not(all(CONFIG_ARCH_AARCH64, feature = "pmu-profiling")))]
pub(crate) fn query(_: u64, _: u64, _: u64, _: &mut [u8]) -> Result<usize> {
    Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
}

#[cfg(not(all(CONFIG_ARCH_AARCH64, feature = "pmu-profiling")))]
pub(crate) fn owns_node(_: &hyper_drivers::platform::PlatformDevice) -> bool {
    false
}
