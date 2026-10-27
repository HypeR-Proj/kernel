// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Static tracepoints with flag-based runtime admission. No instruction patching,
//! timer polling, scheduler queries, formatting or output from producer context.
//! Control/export require explicit CPU inspector profiling authority.

#[cfg(feature = "kernel-trace")]
mod context;
#[cfg(feature = "kernel-trace")]
mod rpc;
#[cfg(feature = "kernel-trace")]
mod runtime;
#[cfg(feature = "kernel-trace")]
pub(crate) use context::{actor, identity, publish_actor};
#[cfg(feature = "kernel-trace")]
pub(crate) use rpc::annotate as annotate_rpc;
#[cfg(feature = "kernel-trace")]
pub use runtime::initialize;
#[cfg(feature = "kernel-trace")]
pub use {hyper_trace::read, hyper_trace::set_filter, hyper_trace::start, hyper_trace::stop};

#[cfg(feature = "kernel-trace")]
mod control;
#[cfg(feature = "kernel-trace")]
pub(crate) use control::{query, start as capture_start, stop as capture_stop};
pub(crate) type Result<T> = core::result::Result<T, hyper::abi::native::HyperNativeStatus>;
#[cfg(not(feature = "kernel-trace"))]
pub(crate) fn capture_start(_: &[u8; 56]) -> Result<u64> {
    Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
}
#[cfg(not(feature = "kernel-trace"))]
pub(crate) fn capture_stop(_: u64) -> Result<()> {
    Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
}
#[cfg(not(feature = "kernel-trace"))]
pub(crate) fn query(_: u64, _: u64, _: u64, _: &mut [u8]) -> Result<usize> {
    Err(hyper::abi::native::HYPER_NATIVE_STATUS_NOT_SUPPORTED)
}
