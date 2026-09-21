// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected host-platform capabilities.
//!
//! Boot policy owns the FDT walk, initialization order, and failure handling.
//! This facade selects the allocation-free essential-device visitor, KASLR
//! geometry, architecture diagnostics, and optional port-I/O executor while
//! keeping backend state private from kernel services.

use hyper::platform::fdt::{NodeId, NodeResources, NodeVisitor, Property};

/// Failure reported by essential architecture-device discovery.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct DiscoveryError(crate::arch::platform::DiscoveryError);

impl core::fmt::Debug for DiscoveryError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Failure selecting an architecture-valid randomized kernel base.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct KaslrError(crate::arch::platform::KaslrError);

impl core::fmt::Debug for KaslrError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Essential platform facts admitted by the selected architecture.
///
/// The wrapper exposes only stable neutral facts to kernel policy. Selected
/// HAL siblings may borrow the private backend value when preparing another
/// machine capability.
#[derive(Clone, Copy, Debug)]
pub struct EssentialInfo(crate::arch::platform::EssentialInfo);

impl EssentialInfo {
    pub fn cpu_power(&self) -> Option<hyper::platform::CpuPowerInfo> {
        self.0.cpu_power()
    }

    pub fn interrupt_controller(&self) -> Option<hyper::platform::InterruptControllerInfo> {
        self.0.interrupt_controller()
    }

    pub fn timer(&self) -> Option<hyper::platform::TimerInfo> {
        self.0.timer()
    }

    pub fn claims(&self) -> &[Option<NodeId>] {
        self.0.claims()
    }

    pub(super) const fn as_backend(&self) -> &crate::arch::platform::EssentialInfo {
        &self.0
    }
}

/// Allocation-free architecture-essential FDT visitor.
///
/// Ordinary discoverable devices remain the platform bus's responsibility;
/// this visitor admits only the interrupt, timer, and CPU-power resources
/// required to reach the runtime driver phase.
pub struct EssentialDiscovery(crate::arch::platform::EssentialDiscovery);

impl Default for EssentialDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

impl EssentialDiscovery {
    pub const fn new() -> Self {
        Self(crate::arch::platform::EssentialDiscovery::new())
    }

    pub fn finish(self) -> Result<EssentialInfo, DiscoveryError> {
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
#[inline]
pub fn select_kernel_base(seed: Option<u64>, image_size: u64) -> Result<u64, KaslrError> {
    crate::arch::platform::select_kernel_base(seed, image_size).map_err(KaslrError)
}

/// Emits selected-architecture runtime descriptions through kernel logging.
#[inline]
pub fn describe_runtime(emit: impl FnMut(core::fmt::Arguments<'_>)) {
    crate::arch::platform::describe_runtime(emit);
}

/// Returns the isolated port-I/O executor supported by this platform, if any.
///
/// The executor does not grant ownership of a port range. Each caller must
/// separately establish that the addressed device belongs to its subsystem.
#[inline]
pub const fn port_io() -> Option<hyper::hal::io::PortIo> {
    crate::arch::platform::port_io()
}

#[derive(Clone, Copy)]
pub struct ProtocolInputs {
    pub dtb_address: usize,
    pub command_line: Option<hyper::platform::chosen::CommandLine>,
    pub initial_ramdisk: Option<hyper::platform::PhysicalRange>,
}

impl ProtocolInputs {
    pub const fn new(
        dtb_address: usize,
        command_line: Option<hyper::platform::chosen::CommandLine>,
        initial_ramdisk: Option<hyper::platform::PhysicalRange>,
    ) -> Self {
        Self {
            dtb_address,
            command_line,
            initial_ramdisk,
        }
    }
}

/// Initialize boot-CPU mechanisms before kernel platform discovery.
///
/// # Safety
/// Arguments must come from the architecture assembly entry with its boot
/// stack, identity mapping, relocation and privileged-state setup complete.
pub unsafe fn prepare_boot(raw0: usize, raw1: usize, ticks: u64) -> ProtocolInputs {
    // SAFETY: The caller establishes the architecture boot entry contract.
    unsafe { crate::arch::platform::prepare_boot(raw0, raw1, ticks) }
}
