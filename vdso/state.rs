// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Private layout shared only by a matching kernel and its embedded vDSO.

use core::sync::atomic::AtomicU64;

#[repr(C)]
pub struct State {
    pub frequency: AtomicU64,
    pub realtime_seconds: AtomicU64,
    pub realtime_nanos: AtomicU64,
    pub realtime_monotonic: AtomicU64,
    pub ipc_active: AtomicU64,
}
