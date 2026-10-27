// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Platform-device discovery and driver lifecycle management.

use alloc::{boxed::Box, string::String, vec::Vec};
use core::mem::ManuallyDrop;

use hyper::mm::VirtualAddress;
use {
    hyper::mm::ranges::PhysicalRange, hyper_platform::fdt::NodeId,
    hyper_platform::fdt::NodeResources, hyper_platform::fdt::NodeVisitor,
    hyper_platform::fdt::Property, hyper_platform::fdt::PropertyError,
};

/// One MMIO register range described by platform firmware.
///
/// This value carries no mapping authority. Drivers must ask
/// [`DriverServices`] to turn it into a [`PermanentMmioMapping`] before
/// accessing registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioResource(PhysicalRange);

impl MmioResource {
    /// Classifies a platform-owned physical range as a device register resource.
    ///
    /// # Safety
    ///
    /// `range` must be a translated MMIO interval described by trusted
    /// platform firmware, and platform policy must have assigned it to the
    /// device whose driver receives this resource. Arbitrary physical ranges
    /// must not be upgraded to mapping authority through this constructor.
    pub const unsafe fn from_physical_range(range: PhysicalRange) -> Self {
        Self(range)
    }

    pub const fn start(self) -> u64 {
        self.0.start()
    }

    pub const fn size(self) -> u64 {
        self.0.size()
    }

    pub const fn end(self) -> u64 {
        self.0.end()
    }

    pub const fn physical_range(self) -> PhysicalRange {
        self.0
    }
}

/// Failure to establish or validate a device mapping capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmioMappingError {
    /// The requested resource is absent from the installed device map.
    NotMapped,
    /// The mapped virtual interval is not representable by the target.
    AddressOverflow,
    /// A driver-required register window does not fit in the resource.
    WindowTooSmall,
    /// The physical or virtual base does not meet the driver's alignment.
    Misaligned,
    /// The requested alignment is zero or is not a power of two.
    InvalidAlignment,
}

/// Authority to access one permanently mapped MMIO resource.
///
/// Stage-1 device mappings currently live for the entire kernel lifetime and
/// cannot be removed. Copies therefore share the same permanent mapping; they
/// do not imply exclusive ownership of the hardware. The subsystem binding a
/// driver remains responsible for serializing register reconfiguration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermanentMmioMapping {
    resource: MmioResource,
    virtual_start: VirtualAddress,
}

impl PermanentMmioMapping {
    /// Creates a capability for an already-installed permanent device mapping.
    ///
    /// # Safety
    ///
    /// The complete `resource` interval must be mapped at `virtual_start` with
    /// device memory attributes for the rest of the kernel lifetime.
    pub unsafe fn new(
        resource: MmioResource,
        virtual_start: VirtualAddress,
    ) -> Result<Self, MmioMappingError> {
        let size =
            usize::try_from(resource.size()).map_err(|_| MmioMappingError::AddressOverflow)?;
        let virtual_start =
            usize::try_from(virtual_start.get()).map_err(|_| MmioMappingError::AddressOverflow)?;
        virtual_start
            .checked_add(size)
            .ok_or(MmioMappingError::AddressOverflow)?;
        Ok(Self {
            resource,
            virtual_start: VirtualAddress::new(virtual_start as u64),
        })
    }

    /// Verifies the minimum register window and base alignment required by a
    /// particular driver binding.
    pub fn validate_window(
        self,
        required_size: u64,
        required_alignment: usize,
    ) -> Result<Self, MmioMappingError> {
        if required_alignment == 0 || !required_alignment.is_power_of_two() {
            return Err(MmioMappingError::InvalidAlignment);
        }
        if required_size == 0 || required_size > self.resource.size() {
            return Err(MmioMappingError::WindowTooSmall);
        }
        let alignment =
            u64::try_from(required_alignment).map_err(|_| MmioMappingError::InvalidAlignment)?;
        if !self.resource.start().is_multiple_of(alignment)
            || !self.virtual_start.get().is_multiple_of(alignment)
        {
            return Err(MmioMappingError::Misaligned);
        }
        Ok(self)
    }

    pub const fn resource(self) -> MmioResource {
        self.resource
    }

    pub const fn virtual_start(self) -> usize {
        self.virtual_start.get() as usize
    }
}

/// A firmware property retained for a discovered platform device.
#[derive(Debug)]
pub struct DeviceProperty {
    name: String,
    value: Vec<u8>,
}

impl DeviceProperty {
    /// Returns the firmware property name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the unmodified firmware property value.
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

/// Resources discovered for one unclaimed, enabled platform device.
#[derive(Debug)]
pub struct PlatformDevice {
    resources_valid: bool,
    kernel_claimed: bool,
    id: NodeId,
    path: String,
    name: String,
    compatibles: Vec<String>,
    registers: Vec<MmioResource>,
    pci_memory: Option<(u64, MmioResource)>,
    interrupt_cells: Vec<u32>,
    properties: Vec<DeviceProperty>,
}

impl PlatformDevice {
    pub const fn kernel_claimed(&self) -> bool {
        self.kernel_claimed
    }
    /// Returns the device tree node identifier.
    pub const fn id(&self) -> NodeId {
        self.id
    }

    /// Returns the canonical full firmware node path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the device tree node name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the compatible strings advertised by the device.
    pub fn compatibles(&self) -> &[String] {
        &self.compatibles
    }

    /// Reports whether the device advertises `compatible`.
    pub fn is_compatible(&self, compatible: &str) -> bool {
        self.compatibles
            .iter()
            .any(|candidate| candidate == compatible)
    }

    /// Returns the physical register ranges described by the device.
    pub fn registers(&self) -> &[MmioResource] {
        &self.registers
    }

    /// The platform-admitted part of a bridge's typed PCI memory window.
    pub const fn pci_memory(&self) -> Option<(u64, MmioResource)> {
        self.pci_memory
    }

    /// All CPU MMIO ownership intervals, including a bridge's admitted memory
    /// aperture. Alias checks must cover BARs without individual DT leaf nodes.
    pub fn mmio_resources(&self) -> impl Iterator<Item = MmioResource> + '_ {
        self.registers
            .iter()
            .copied()
            .chain(self.pci_memory.map(|(_, range)| range))
    }

    /// Returns cells that must be translated by the parent IRQ domain.
    pub fn interrupt_cells(&self) -> &[u32] {
        &self.interrupt_cells
    }

    /// Returns unrecognized, unnormalized firmware properties.
    pub fn properties(&self) -> &[DeviceProperty] {
        &self.properties
    }

    /// Returns an unnormalized firmware property.
    ///
    /// Core binding data such as `compatible` and `interrupts` is exposed by
    /// dedicated accessors and is not duplicated in this collection.
    pub fn property(&self, name: &str) -> Option<&[u8]> {
        self.properties
            .iter()
            .find(|property| property.name == name)
            .map(|property| property.value.as_slice())
    }
}

/// Errors reported while scanning platform devices from firmware data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanError {
    /// Memory could not be reserved for copied firmware data.
    Allocation,
    /// A property did not have the format required by this scanner.
    MalformedProperty,
    /// Firmware node nesting was invalid.
    MalformedTree,
    /// A string property was not valid UTF-8.
    InvalidUtf8,
}

struct DeviceBuilder {
    id: NodeId,
    name: String,
    compatibles: Vec<String>,
    interrupt_cells: Vec<u32>,
    properties: Vec<DeviceProperty>,
}

/// Heap-backed visitor used only after the global allocator is installed.
///
/// The scanner copies properties while a node is open and retains the copies
/// only for enabled, compatible, unclaimed devices. Its peak allocation is
/// therefore proportional to nesting depth plus the retained device data.
pub struct DeviceScanner<'a> {
    dependencies: bool,
    claims: &'a [Option<NodeId>],
    stack: Vec<DeviceBuilder>,
    devices: Vec<PlatformDevice>,
}

impl<'a> DeviceScanner<'a> {
    /// Creates a scanner that excludes the supplied claimed node identifiers.
    pub const fn new(claims: &'a [Option<NodeId>]) -> Self {
        Self {
            claims,
            dependencies: false,
            stack: Vec::new(),
            devices: Vec::new(),
        }
    }

    /// Captures a temporary firmware dependency graph, including nodes already
    /// owned by boot drivers. Such nodes remain marked and cannot be assigned.
    pub const fn for_dependency_graph(claims: &'a [Option<NodeId>]) -> Self {
        Self {
            dependencies: true,
            claims,
            stack: Vec::new(),
            devices: Vec::new(),
        }
    }

    /// Completes the scan and returns the discovered devices.
    ///
    /// # Errors
    ///
    /// Returns the first scan error or [`ScanError::MalformedTree`] when nodes
    /// remain open.
    pub fn finish(self) -> Result<Vec<PlatformDevice>, ScanError> {
        if self.stack.is_empty() {
            Ok(self.devices)
        } else {
            Err(ScanError::MalformedTree)
        }
    }

    fn is_claimed(&self, id: NodeId) -> bool {
        self.claims.iter().flatten().any(|claim| *claim == id)
    }

    fn record_property(&mut self, property: Property<'_>) -> Result<(), ScanError> {
        let builder = self.stack.last_mut().ok_or(ScanError::MalformedTree)?;
        let name = property.name();
        if name == "compatible" {
            for compatible in property
                .strings()
                .map_err(map_string_error)?
                .filter(|candidate| !candidate.is_empty())
            {
                builder
                    .compatibles
                    .try_reserve(1)
                    .map_err(|_| ScanError::Allocation)?;
                builder.compatibles.push(copy_string(compatible)?);
            }
        }
        if name == "interrupts"
            && let Ok(cells) = property.cells()
        {
            // Resource decoding is quarantined per node by the FDT collector.
            // A dependency snapshot separately retains the original bytes,
            // even when they cannot grant a host interrupt resource.
            builder
                .interrupt_cells
                .try_reserve_exact(property.bytes().len() / 4)
                .map_err(|_| ScanError::Allocation)?;
            builder.interrupt_cells.extend(cells);
        }
        if !self.dependencies && matches!(name, "compatible" | "interrupts") {
            return Ok(());
        }
        builder
            .properties
            .try_reserve(1)
            .map_err(|_| ScanError::Allocation)?;
        let mut property_value = Vec::new();
        property_value
            .try_reserve_exact(property.bytes().len())
            .map_err(|_| ScanError::Allocation)?;
        property_value.extend_from_slice(property.bytes());
        builder.properties.push(DeviceProperty {
            name: copy_string(name)?,
            value: property_value,
        });
        Ok(())
    }

    fn complete_node(&mut self, node: NodeResources<'_>) -> Result<(), ScanError> {
        let builder = self.stack.pop().ok_or(ScanError::MalformedTree)?;
        if (!node.enabled && !self.dependencies)
            || (!self.dependencies && (builder.compatibles.is_empty() || self.is_claimed(node.id)))
            || self.stack.is_empty()
        {
            return Ok(());
        }
        self.devices
            .try_reserve(1)
            .map_err(|_| ScanError::Allocation)?;
        let mut registers = Vec::new();
        registers
            .try_reserve_exact(node.registers.len())
            .map_err(|_| ScanError::Allocation)?;
        // ResourceCollector translated these ranges through every parent bus
        // before publishing the completed node. Keeping the tuple constructor
        // private makes DeviceScanner the normal capability minting point.
        // The dependency snapshot also preserves disabled firmware subtrees.
        // They describe guest devices but grant no host MMIO authority.
        if node.enabled {
            registers.extend(node.registers.iter().copied().map(MmioResource));
        }
        let mut path = String::new();
        let length = self
            .stack
            .iter()
            .map(|parent| parent.name.len() + 1)
            .sum::<usize>()
            + builder.name.len();
        path.try_reserve_exact(length)
            .map_err(|_| ScanError::Allocation)?;
        for parent in &self.stack {
            if !parent.name.is_empty() {
                path.push('/');
                path.push_str(&parent.name);
            }
        }
        path.push('/');
        path.push_str(&builder.name);
        self.devices.push(PlatformDevice {
            resources_valid: node.enabled && node.resource_error.is_none(),
            kernel_claimed: self.is_claimed(node.id),
            id: builder.id,
            path,
            name: builder.name,
            compatibles: builder.compatibles,
            registers,
            pci_memory: node
                .enabled
                .then_some(node.pci_memory)
                .flatten()
                .map(|(bus, range)| (bus, MmioResource(range))),
            interrupt_cells: if node.enabled && node.resource_error.is_none() {
                builder.interrupt_cells
            } else {
                Vec::new()
            },
            properties: builder.properties,
        });
        Ok(())
    }
}

impl NodeVisitor for DeviceScanner<'_> {
    type Error = ScanError;

    fn begin_node(&mut self, id: NodeId, name: &str) -> Result<(), Self::Error> {
        self.stack
            .try_reserve(1)
            .map_err(|_| ScanError::Allocation)?;
        self.stack.push(DeviceBuilder {
            id,
            name: copy_string(name)?,
            compatibles: Vec::new(),
            interrupt_cells: Vec::new(),
            properties: Vec::new(),
        });
        Ok(())
    }

    fn property(&mut self, _id: NodeId, property: Property<'_>) -> Result<(), Self::Error> {
        self.record_property(property)
    }

    fn end_node(&mut self, node: NodeResources<'_>) -> Result<(), Self::Error> {
        self.complete_node(node)
    }
}

fn map_string_error(error: PropertyError) -> ScanError {
    match error {
        PropertyError::InvalidUtf8 => ScanError::InvalidUtf8,
        PropertyError::EmbeddedTerminator
        | PropertyError::InvalidLength
        | PropertyError::MissingTerminator => ScanError::MalformedProperty,
    }
}

fn copy_string(value: &str) -> Result<String, ScanError> {
    let mut result = String::new();
    result
        .try_reserve_exact(value.len())
        .map_err(|_| ScanError::Allocation)?;
    result.push_str(value);
    Ok(result)
}

/// Errors returned when registering, probing, or managing a driver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeError {
    /// The driver requires another device to bind first.
    Deferred,
    /// Required memory or hardware resources were unavailable.
    Resource,
    /// No supported binding is available for the device.
    Unsupported,
    /// The driver reported an implementation-defined failure code.
    Driver(i32),
}

/// Services provided by the kernel to platform drivers.
pub trait DriverServices {
    /// Returns the permanent device mapping for a described MMIO resource.
    ///
    /// The current stage-1 implementation has no unmap operation. A successful
    /// capability therefore remains valid even after a driver instance is
    /// removed.
    fn map_mmio(&self, resource: MmioResource) -> Result<PermanentMmioMapping, MmioMappingError>;
}

/// A bound platform-driver instance.
pub trait DriverInstance: Send {
    /// Makes a fully prepared instance externally active.
    ///
    /// The driver manager owns the instance before calling this method. Probe
    /// must leave interrupt sources, DMA, and externally callable callbacks
    /// quiesced; a failed activation must remain safe for [`Self::remove`].
    fn activate(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }

    /// Suspends the device before a system-wide low-power transition.
    fn suspend(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }

    /// Resumes the device after a successful suspension.
    fn resume(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }

    /// Releases driver-owned activity before the instance is dropped.
    ///
    /// Permanent stage-1 mappings are not released because the memory manager
    /// does not currently support device unmapping. The default is valid only
    /// for instances whose activation did not enable hardware or publish
    /// callbacks; active drivers must override it and quiesce those resources
    /// first.
    fn remove(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }
}

/// Matches and binds a platform device.
pub trait PlatformDriver: Sync {
    /// Returns the stable name used to identify this driver.
    fn name(&self) -> &'static str;

    /// Returns the device tree compatible strings supported by this driver.
    fn compatible_table(&self) -> &'static [&'static str];

    /// Prepares a quiescent instance for a discovered device.
    ///
    /// Returning `Ok` transfers every acquired resource to the instance, but
    /// must not yet expose DMA or callbacks. Returning `Err` must roll back all
    /// resources acquired by that attempt. The manager reserves ownership
    /// storage before calling probe and invokes [`DriverInstance::activate`]
    /// only after the instance is owned by a binding.
    fn probe(
        &self,
        device: &PlatformDevice,
        services: &dyn DriverServices,
    ) -> Result<Box<dyn DriverInstance>, ProbeError>;
}

struct Binding {
    device_id: NodeId,
    driver_name: &'static str,
    instance: Box<dyn DriverInstance>,
    lifecycle: BindingLifecycle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BindingLifecycle {
    Prepared,
    Active,
    Suspended,
}

/// The outcome of a platform-device probing pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProbeReport {
    /// Number of devices successfully bound to a driver.
    pub bound: usize,
    /// Number of devices for which no driver matched.
    pub unmatched: usize,
    /// Number of devices still waiting for a dependency.
    pub deferred: usize,
    /// Number of devices whose driver probe failed.
    pub failed: usize,
}

/// Registry of platform drivers and their bound instances.
#[must_use = "installed driver bindings require explicit retirement or permanent retention"]
pub struct DriverManager {
    drivers: Vec<&'static dyn PlatformDriver>,
    bindings: Vec<Binding>,
}

/// A driver manager installed in kernel-lifetime storage.
///
/// This type deliberately has no teardown API. Runtime owners which require a
/// finite lifetime must retain [`DriverManager`] and use explicit removal.
#[must_use = "a permanent driver manager must be moved into permanent storage"]
pub struct PermanentDriverManager {
    _manager: ManuallyDrop<DriverManager>,
}

impl DriverManager {
    /// Creates an empty driver registry.
    pub const fn new() -> Self {
        Self {
            drivers: Vec::new(),
            bindings: Vec::new(),
        }
    }

    /// Registers a driver for future device probing.
    ///
    /// # Errors
    ///
    /// Returns [`ProbeError::Driver`] if another driver has the same name or
    /// [`ProbeError::Resource`] if the registry cannot grow.
    pub fn register(&mut self, driver: &'static dyn PlatformDriver) -> Result<(), ProbeError> {
        if self
            .drivers
            .iter()
            .any(|registered| registered.name() == driver.name())
        {
            return Err(ProbeError::Driver(-1));
        }
        self.drivers
            .try_reserve(1)
            .map_err(|_| ProbeError::Resource)?;
        self.drivers.push(driver);
        Ok(())
    }

    /// Probes every unbound device and retries drivers whose dependencies defer.
    pub fn probe_devices(
        &mut self,
        devices: &[PlatformDevice],
        services: &dyn DriverServices,
    ) -> ProbeReport {
        let mut report = ProbeReport::default();
        let mut deferred = Vec::new();
        for (index, device) in devices.iter().enumerate() {
            if self
                .bindings
                .iter()
                .any(|binding| binding.device_id == device.id())
            {
                continue;
            }
            let Some(driver) = self.matching_driver(device) else {
                report.unmatched += 1;
                continue;
            };
            match self.bind(driver, device, services) {
                Ok(()) => report.bound += 1,
                Err(ProbeError::Deferred) => {
                    if deferred.try_reserve(1).is_ok() {
                        deferred.push(index);
                    } else {
                        report.failed += 1;
                    }
                }
                Err(_) => report.failed += 1,
            }
        }
        self.retry_deferred(devices, services, deferred, &mut report);
        report
    }

    fn retry_deferred(
        &mut self,
        devices: &[PlatformDevice],
        services: &dyn DriverServices,
        mut deferred: Vec<usize>,
        report: &mut ProbeReport,
    ) {
        while !deferred.is_empty() {
            let mut next = Vec::new();
            if next.try_reserve_exact(deferred.len()).is_err() {
                report.failed += deferred.len();
                return;
            }
            let mut progress = false;
            for index in deferred {
                let Some(device) = devices.get(index) else {
                    report.failed += 1;
                    continue;
                };
                let Some(driver) = self.matching_driver(device) else {
                    report.unmatched += 1;
                    continue;
                };
                match self.bind(driver, device, services) {
                    Ok(()) => {
                        report.bound += 1;
                        progress = true;
                    }
                    Err(ProbeError::Deferred) => next.push(index),
                    Err(_) => report.failed += 1,
                }
            }
            if !progress {
                report.deferred += next.len();
                return;
            }
            deferred = next;
        }
    }

    fn matching_driver(&self, device: &PlatformDevice) -> Option<&'static dyn PlatformDriver> {
        self.drivers.iter().copied().find(|driver| {
            driver
                .compatible_table()
                .iter()
                .any(|compatible| device.is_compatible(compatible))
        })
    }

    fn bind(
        &mut self,
        driver: &'static dyn PlatformDriver,
        device: &PlatformDevice,
        services: &dyn DriverServices,
    ) -> Result<(), ProbeError> {
        if !device.resources_valid {
            return Err(ProbeError::Resource);
        }
        // Reserve the publication slot before probe can acquire resources.
        // Once probe succeeds, insertion is infallible and the manager owns
        // the instance before any activation can expose callbacks or DMA.
        self.bindings
            .try_reserve(1)
            .map_err(|_| ProbeError::Resource)?;
        let instance = driver.probe(device, services)?;
        self.bindings.push(Binding {
            device_id: device.id(),
            driver_name: driver.name(),
            instance,
            lifecycle: BindingLifecycle::Prepared,
        });
        let index = self.bindings.len() - 1;
        match self.bindings[index].instance.activate() {
            Ok(()) => {
                self.bindings[index].lifecycle = BindingLifecycle::Active;
                Ok(())
            }
            Err(error) => {
                self.retire_failed_binding(index);
                Err(error)
            }
        }
    }

    /// Suspends bound instances in reverse binding order.
    ///
    /// # Errors
    ///
    /// Returns the error from the first instance that fails to suspend.
    pub fn suspend_all(&mut self) -> Result<(), ProbeError> {
        for index in (0..self.bindings.len()).rev() {
            match self.bindings[index].lifecycle {
                BindingLifecycle::Suspended => continue,
                BindingLifecycle::Active => {}
                BindingLifecycle::Prepared => return Err(ProbeError::Driver(-2)),
            }
            if let Err(error) = self.bindings[index].instance.suspend() {
                // Restore the pre-suspend state on a best-effort basis.
                // Per-binding state makes a later resume retry safe even if
                // rollback itself encounters a device failure.
                let _ = self.resume_all();
                return Err(error);
            }
            self.bindings[index].lifecycle = BindingLifecycle::Suspended;
        }
        Ok(())
    }

    /// Resumes suspended instances in binding order.
    ///
    /// # Errors
    ///
    /// Returns the error from the first instance that fails to resume.
    pub fn resume_all(&mut self) -> Result<(), ProbeError> {
        for binding in &mut self.bindings {
            if binding.lifecycle == BindingLifecycle::Suspended {
                binding.instance.resume()?;
                binding.lifecycle = BindingLifecycle::Active;
            } else if binding.lifecycle == BindingLifecycle::Prepared {
                return Err(ProbeError::Driver(-2));
            }
        }
        Ok(())
    }

    /// Requests teardown and drops the instance bound to `device_id`.
    ///
    /// Returns `false` if no driver is bound to the device.
    ///
    /// # Errors
    ///
    /// Returns an error if the driver cannot remove its instance.
    pub fn remove(&mut self, device_id: NodeId) -> Result<bool, ProbeError> {
        let Some(index) = self
            .bindings
            .iter()
            .position(|binding| binding.device_id == device_id)
        else {
            return Ok(false);
        };
        self.bindings[index].instance.remove()?;
        self.bindings.remove(index);
        Ok(true)
    }

    /// Explicitly retires every installed instance in reverse binding order.
    ///
    /// Failure preserves the failing instance and every earlier binding so the
    /// owner can retry from a context where device teardown is permitted.
    pub fn retire_all(&mut self) -> Result<(), ProbeError> {
        while let Some(index) = self.bindings.len().checked_sub(1) {
            self.bindings[index].instance.remove()?;
            let _ = self.bindings.pop();
        }
        Ok(())
    }

    /// Converts all installed bindings to explicit kernel-lifetime ownership.
    pub fn retain_permanently(self) -> PermanentDriverManager {
        PermanentDriverManager {
            _manager: ManuallyDrop::new(self),
        }
    }

    /// Returns the driver name bound to `device_id`, if any.
    pub fn binding_driver(&self, device_id: NodeId) -> Option<&'static str> {
        self.bindings
            .iter()
            .find(|binding| binding.device_id == device_id)
            .map(|binding| binding.driver_name)
    }

    /// Retires an instance whose activation failed after manager ownership was
    /// established.
    ///
    /// A failed remove cannot justify dropping callback context which hardware
    /// might still reach. The rollback helper therefore retains the exact
    /// instance on a fail-stop stack rather than returning ambiguous state.
    fn retire_failed_binding(&mut self, index: usize) {
        let binding = self.bindings.remove(index);
        rollback_or_fail_stop(binding.instance);
    }
}

impl Default for DriverManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DriverManager {
    fn drop(&mut self) {
        if !self.bindings.is_empty() {
            // Drop can run under unrelated locks or with IRQs masked, so it
            // must neither invoke arbitrary driver teardown nor diagnostics.
            // Silently leaking would hide a missed ownership transition. A
            // manager with live bindings must instead be installed for the
            // kernel lifetime or retired explicitly before destruction.
            ownership_violation()
        }
    }
}

impl Drop for PermanentDriverManager {
    fn drop(&mut self) {
        // Static kernel ownership has no destruction phase. Reaching Drop
        // means a supposedly permanent owner was abandoned; do not call
        // drivers or diagnostics from an unknown lock/IRQ context.
        ownership_violation()
    }
}

fn rollback_or_fail_stop(mut instance: Box<dyn DriverInstance>) {
    if instance.remove().is_err() {
        // The device may retain a raw callback context or DMA reference into
        // the instance. Returning would let the kernel continue beside an
        // ambiguously active device, while dropping would violate memory
        // safety. Keep the owner live on this fail-stop stack instead.
        ownership_violation()
    }
}

fn ownership_violation() -> ! {
    // The binary panic handler owns crash policy. Keep the live owner on
    // this stack: no ordinary locks, allocation, or unwinding is permitted.
    hyper::debug::invariant_failure("platform device ownership invariant")
}
