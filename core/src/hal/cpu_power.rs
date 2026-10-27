// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

/// Firmware-visible identifier of a hardware processing element.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CpuHardwareId(u64);

impl CpuHardwareId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Physical entry address used when a powered CPU resumes execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResumeAddress(u64);

impl ResumeAddress {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Opaque firmware power-state encoding discovered from platform data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SuspendState(u32);

impl SuspendState {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuAffinityState {
    On,
    Off,
    OnPending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuPowerVersion {
    pub major: u16,
    pub minor: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuPowerCapabilities {
    pub version: CpuPowerVersion,
    pub cpu_suspend: bool,
    pub cpu_off: bool,
    pub cpu_on: bool,
    pub affinity_info: bool,
    pub system_off: bool,
    pub system_reset: bool,
}

/// Architecture-neutral firmware CPU and system power operations.
pub trait CpuPower {
    type Error;

    fn capabilities(&self) -> CpuPowerCapabilities;

    /// Starts a powered-off CPU at an architecture resume entry.
    ///
    /// # Safety
    ///
    /// `entry` must name a valid resume trampoline for the target CPU.
    /// `context` must satisfy that trampoline's address, lifetime, alignment,
    /// initialization, and cache-coherency requirements until the target has
    /// consumed it. The target must not already be executing from the same
    /// context record.
    unsafe fn cpu_on(
        &self,
        target: CpuHardwareId,
        entry: ResumeAddress,
        context: u64,
    ) -> Result<(), Self::Error>;

    /// Powers off the calling CPU. Success does not return.
    fn cpu_off(&self) -> Result<(), Self::Error>;

    /// Suspends the calling CPU. A powerdown state resumes at `entry`.
    ///
    /// # Safety
    ///
    /// For a powerdown state, `entry` and `context` must satisfy the same
    /// resume-trampoline validity and lifetime requirements as [`Self::cpu_on`].
    unsafe fn cpu_suspend(
        &self,
        state: SuspendState,
        entry: ResumeAddress,
        context: u64,
    ) -> Result<(), Self::Error>;

    fn affinity_info(
        &self,
        target: CpuHardwareId,
        lowest_affinity_level: u8,
    ) -> Result<CpuAffinityState, Self::Error>;

    /// Powers off the complete system. Success does not return.
    fn system_off(&self) -> Result<(), Self::Error>;

    /// Resets the complete system. Success does not return.
    fn system_reset(&self) -> Result<(), Self::Error>;
}
