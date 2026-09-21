// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected-architecture host platform integration.
//!
//! Common boot policy owns DTB walking, initialization order, and failure
//! handling. This facade selects the allocation-free discovery visitor,
//! architecture KASLR geometry, optional port-I/O capability, and diagnostic
//! architecture report without exposing a backend module.

use hyper::platform::fdt::{NodeId, NodeResources, NodeVisitor, Property};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiscoveryError(super::imp::PlatformDiscoveryError);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KaslrError {
    InvalidImage,
    Selection(super::imp::KaslrError),
}

/// Architecture discovery result narrowed to capabilities consumed by policy.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EssentialInfo(super::imp::EssentialPlatformInfo);

impl EssentialInfo {
    pub(crate) fn cpu_power(&self) -> Option<hyper::platform::CpuPowerInfo> {
        self.0.cpu_power
    }

    pub(crate) fn interrupt_controller(&self) -> Option<hyper::platform::InterruptControllerInfo> {
        self.0.interrupt_controller
    }

    pub(crate) fn timer(&self) -> Option<hyper::platform::TimerInfo> {
        self.0.timer
    }

    pub(crate) fn claims(&self) -> &[Option<NodeId>] {
        self.0.claims()
    }

    pub(super) const fn as_backend(&self) -> &super::imp::EssentialPlatformInfo {
        &self.0
    }
}

/// Allocation-free essential-device visitor for the selected architecture.
pub(crate) struct EssentialDiscovery(super::imp::EssentialDeviceDiscovery);

impl EssentialDiscovery {
    pub(crate) const fn new() -> Self {
        Self(super::imp::EssentialDeviceDiscovery::new())
    }

    pub(crate) fn finish(self) -> Result<EssentialInfo, DiscoveryError> {
        self.0.finish().map(EssentialInfo).map_err(DiscoveryError)
    }
}

impl NodeVisitor for EssentialDiscovery {
    type Error = DiscoveryError;

    fn begin_node(&mut self, id: NodeId, name: &str) -> Result<(), Self::Error> {
        self.0.begin_node(id, name).map_err(DiscoveryError)
    }

    fn property(&mut self, id: NodeId, property: Property<'_>) -> Result<(), Self::Error> {
        self.0.property(id, property).map_err(DiscoveryError)
    }

    fn end_node(&mut self, node: NodeResources<'_>) -> Result<(), Self::Error> {
        self.0.end_node(node).map_err(DiscoveryError)
    }
}

/// Selects the architecture-valid virtual base for the loaded kernel image.
///
/// Backends retain their own randomization windows and result layouts; common
/// boot policy consumes only the selected base shared by every architecture.
#[inline]
pub(crate) fn select_kernel_base(seed: Option<u64>, image_size: u64) -> Result<u64, KaslrError> {
    if image_size == 0 {
        return Err(KaslrError::InvalidImage);
    }
    super::imp::select_kaslr_layout(seed, image_size)
        .map(|layout| layout.kernel_base)
        .map_err(KaslrError::Selection)
}

/// Visits architecture-specific runtime descriptions without choosing a log
/// sink below kernel policy. Formatting arguments borrow only immutable
/// backend state for the duration of each allocation-free callback.
#[inline]
pub(crate) fn describe_runtime(emit: impl FnMut(core::fmt::Arguments<'_>)) {
    super::imp::describe_runtime(emit);
}

/// Issues the selected architecture's isolated port-I/O capability, if any.
///
/// Possessing this executor does not grant ownership of every port. Callers
/// must still establish ownership of each port range before using it.
#[inline]
pub(crate) const fn port_io() -> Option<hyper::hal::io::PortIo> {
    super::imp::port_io()
}

/// # Safety
/// Called only after architecture assembly entry establishes boot state.
pub(crate) unsafe fn prepare_boot(
    raw0: usize,
    raw1: usize,
    ticks: u64,
) -> crate::hal::platform::ProtocolInputs {
    // SAFETY: The selected backend consumes the same assembly boot arguments.
    unsafe { super::imp::prepare_boot(raw0, raw1, ticks) }
}
