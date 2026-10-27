// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Platform-device enumeration and driver binding orchestration.

use alloc::vec::Vec;
use core::mem::ManuallyDrop;

use {
    hyper::sync::InterruptSpinLock, hyper_drivers::platform::DeviceScanner,
    hyper_drivers::platform::DriverManager, hyper_drivers::platform::DriverServices,
    hyper_drivers::platform::MmioMappingError, hyper_drivers::platform::MmioResource,
    hyper_drivers::platform::PermanentDriverManager, hyper_drivers::platform::PermanentMmioMapping,
    hyper_drivers::platform::PlatformDevice, hyper_drivers::platform::PlatformDriver,
    hyper_drivers::platform::ProbeError, hyper_drivers::platform::ProbeReport,
    hyper_drivers::platform::ScanError, hyper_platform::fdt,
};

type KernelSpinLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

struct KernelDriverServices<'a> {
    boot: &'a crate::boot::Initialization,
}

impl DriverServices for KernelDriverServices<'_> {
    fn map_mmio(&self, resource: MmioResource) -> Result<PermanentMmioMapping, MmioMappingError> {
        if !self.boot.maps_mmio(resource) {
            return Err(MmioMappingError::NotMapped);
        }
        let virtual_start = crate::mm::memory::mmio_address(resource.start())
            .ok_or(MmioMappingError::AddressOverflow)?;
        // SAFETY: Final stage-1 construction maps every DTB-discovered MMIO
        // range with device attributes and retains those mappings permanently.
        unsafe {
            PermanentMmioMapping::new(
                resource,
                hyper::mm::VirtualAddress::new(virtual_start as u64),
            )
        }
    }
}

struct PlatformBusState {
    _iommu: Option<hyper::mm::FallibleArc<super::iommu::runtime::Runtime>>,
    _devices: Vec<PlatformDevice>,
    _manager: PermanentDriverManager,
    assignable: Vec<super::assigned::Resource>,
    catalogue: hyper::mm::FallibleArc<super::firmware::Catalogue>,
}

enum PlatformBusLifecycle {
    Empty,
    Preparing,
    Ready { _state: PlatformBusState },
}

struct InitializationReservation {
    active: bool,
}

static PLATFORM_BUS: KernelSpinLock<PlatformBusLifecycle> =
    KernelSpinLock::new(PlatformBusLifecycle::Empty);
static BUILTIN_DRIVERS: &[&dyn PlatformDriver] = &[
    &hyper_drivers::serial::PL011_PLATFORM_DRIVER,
    &hyper_drivers::serial::NS16550_PLATFORM_DRIVER,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    AlreadyInitialized,
    InitializationInProgress,
    DriverRegistration(ProbeError),
    Fdt(fdt::Error),
    Scan(ScanError),
    Iommu(super::iommu::Error),
    Pci(hyper_drivers::pci::Error),
    PciOwnership,
}

pub(super) struct InitializationReport {
    pub(super) drivers: ProbeReport,
    pub(super) console: Result<Option<super::serial::Capabilities>, super::serial::Error>,
}

pub(super) fn initialize(
    boot: &crate::boot::Initialization,
) -> Result<InitializationReport, Error> {
    let reservation = InitializationReservation::acquire()?;
    let mut scanner = DeviceScanner::new(boot.essential().claims());
    // SAFETY: The DTB reservation remains in the permanent RAM linear map.
    match unsafe { fdt::discover_with(boot.linear_dtb(), &mut scanner) } {
        Ok(_) => {}
        Err(fdt::WalkError::Fdt(error)) => return Err(Error::Fdt(error)),
        Err(fdt::WalkError::Visitor(error)) => return Err(Error::Scan(error)),
    }
    let mut devices = scanner.finish().map_err(Error::Scan)?;
    let mut dependency_scanner = DeviceScanner::for_dependency_graph(boot.essential().claims());
    // SAFETY: Same permanently reserved firmware blob as the primary scan.
    match unsafe { fdt::discover_with(boot.linear_dtb(), &mut dependency_scanner) } {
        Ok(_) => {}
        Err(fdt::WalkError::Fdt(error)) => return Err(Error::Fdt(error)),
        Err(fdt::WalkError::Visitor(error)) => return Err(Error::Scan(error)),
    }
    let dependencies = dependency_scanner.finish().map_err(Error::Scan)?;
    // Complete every fallible framework allocation before activating the
    // runtime console. From that publication onward initialization has no
    // recoverable error path which could discard its IRQ ownership.
    let mut manager = DriverManager::new();
    for &driver in BUILTIN_DRIVERS {
        manager
            .register(driver)
            .map_err(Error::DriverRegistration)?;
    }
    let mut assignable = Vec::new();
    assignable
        .try_reserve_exact(devices.len().saturating_add(dependencies.len()))
        .map_err(|_| {
            crate::pr_err!(
                "HypeR: physical device catalogue allocation failed ({} nodes, {} bytes per entry)",
                devices.len() + dependencies.len(),
                core::mem::size_of::<super::assigned::Resource>()
            );
            Error::DriverRegistration(ProbeError::Resource)
        })?;
    let catalogue = super::firmware::Catalogue::new(dependencies, |node| node.kernel_claimed())
        .map_err(|_| {
            crate::pr_err!("HypeR: firmware catalogue allocation failed");
            Error::DriverRegistration(ProbeError::Resource)
        })?;
    #[cfg(all(CONFIG_ARCH_AARCH64, feature = "pmu-profiling"))]
    crate::diagnostics::profile::initialize(&catalogue.nodes, boot.interrupts().root_domain);
    let services = KernelDriverServices { boot };
    // Activate default-deny translation before publishing any physical device.
    // A discovered but unsupported/failed IOMMU aborts admission, never bypasses.
    let iommu =
        super::iommu::initialize(&catalogue.nodes, &services, boot.interrupts().root_domain)
            .map_err(Error::Iommu)?;
    // ECAM functions require the already active default-deny SMMU. A PCI
    // configuration error is fatal to admission rather than a DMA bypass.
    if catalogue
        .nodes
        .iter()
        .any(|node| node.is_compatible("pci-host-ecam-generic"))
        && iommu.is_some()
    {
        let functions =
            hyper_drivers::pci::Transport::discover_ecam::<crate::hal::memory::Barrier>(
                &catalogue.nodes,
                &services,
            )
            .map_err(Error::Pci)?;
        // Keep later catalogue pushes infallible after runtime driver IRQs
        // are active, including all ECAM functions beyond firmware nodes.
        assignable
            .try_reserve_exact(
                devices
                    .len()
                    .saturating_add(catalogue.nodes.len())
                    .saturating_add(functions.len()),
            )
            .map_err(|_| Error::DriverRegistration(ProbeError::Resource))?;
        for prepared in functions {
            let identity = prepared.transport.identity();
            let stream = prepared.transport.stream();
            let resource = super::assigned::Resource::pci(prepared, boot.interrupts().root_domain)
                .ok_or(Error::PciOwnership)?;
            assignable.push(resource);
            crate::pr_info!(
                "HypeR: PCI function {:04x}:{:04x} prepared; SMMU stream {:?}, DMA disabled",
                identity & 0xffff,
                identity >> 16,
                stream
            );
        }
    }
    let console = super::serial::initialize(boot, &devices);
    let reserved_console_base = boot.early_console().map(|console| console.base);
    devices.retain(|device| {
        reserved_console_base.is_none_or(|reserved| {
            device.registers().first().map(|range| range.start()) != Some(reserved)
        })
    });
    crate::time::initialize_realtime(&devices, &services);
    let drivers = manager.probe_devices(&devices, &services);
    // The PCI function owns its bridge/MSI/dependency tree. Firmware
    // must have quiesced DMA before Image entry; preparation gates bus mastering
    // and does not expose any packet DMA until an assigned VM owns its backing.
    let kernel_owned = |node: &PlatformDevice| {
        node.kernel_claimed()
            || crate::diagnostics::profile::owns_node(node)
            || super::iommu::host_owned(&catalogue.nodes, node)
            || manager.binding_driver(node.id()).is_some()
            || reserved_console_base.is_some_and(|base| {
                node.registers()
                    .iter()
                    .any(|range| range.start() <= base && base < range.end())
            })
    };
    // Include aliases outside the PCI tree before the transport touches MMIO.
    // Catalogue reservation checks physical pages and interrupt identities.
    catalogue.reserve(kernel_owned);
    let pci_conflict = catalogue.nodes.iter().enumerate().find(|(index, node)| {
        hyper_drivers::pci::owns_handoff_node(&catalogue.nodes, node) && catalogue.reserved(*index)
    });
    if let Some((_, node)) = pci_conflict {
        crate::pr_warn!(
            "HypeR: PCI function handoff conflicts with a kernel device: {}",
            node.path()
        );
    } else {
        match hyper_drivers::pci::Transport::discover::<crate::hal::memory::Barrier>(
            &catalogue.nodes,
            &services,
        ) {
            Ok(Some(prepared)) => {
                let identity = prepared.transport.identity();
                if let Some(resource) =
                    super::assigned::Resource::pci(prepared, boot.interrupts().root_domain)
                {
                    assignable.push(resource);
                    crate::pr_info!(
                        "HypeR: PCI function {:04x}:{:04x} prepared; DMA windows configured",
                        identity & 0xffff,
                        identity >> 16
                    );
                } else {
                    crate::pr_warn!("HypeR: PCI function ownership allocation unavailable");
                }
            }
            Ok(None) => {}
            Err(error) => crate::pr_warn!("HypeR: PCI function handoff unavailable: {error:?}"),
        }
    }
    catalogue.reserve(|node| {
        kernel_owned(node) || hyper_drivers::pci::owns_handoff_node(&catalogue.nodes, node)
    });
    for device in &devices {
        if manager.binding_driver(device.id()).is_none()
            && catalogue
                .nodes
                .iter()
                .position(|node| node.id() == device.id())
                .is_some_and(|index| !catalogue.reserved(index))
            && device.is_compatible("virtio,mmio")
            && let Some(resource) = super::assigned::Resource::discover(
                device,
                &services,
                boot.interrupts().root_domain,
            )
        {
            assignable.push(resource);
        }
    }
    for (index, node) in catalogue.nodes.iter().enumerate() {
        if !catalogue.reserved(index)
            && let Some(resource) = super::assigned::Resource::discover_userspace(
                node,
                &services,
                boot.interrupts().root_domain,
            )
        {
            assignable.push(resource);
        }
    }
    reservation.commit(PlatformBusState {
        _iommu: iommu,
        _devices: devices,
        _manager: manager.retain_permanently(),
        assignable,
        catalogue,
    });
    Ok(InitializationReport { drivers, console })
}

pub(super) fn claim(index: usize) -> Option<super::assigned::Claim> {
    PLATFORM_BUS.with(|state| match state {
        PlatformBusLifecycle::Ready { _state: state } => {
            let resource = state.assignable.get(index)?;
            if !super::assigned::transport_profile(resource.profile()) {
                return None;
            }
            if state
                .assignable
                .iter()
                .enumerate()
                .any(|(other_index, other)| {
                    other_index != index && other.claimed() && other.conflicts(resource)
                })
            {
                return None;
            }
            state.assignable.get_mut(index)?.claim(index)
        }
        _ => None,
    })
}

pub(super) fn claim_matching(
    profile: u32,
    identity_kind: u32,
    identity: &str,
) -> Result<super::assigned::Claim, super::assigned::service::MatchError> {
    use super::assigned::service::MatchError;
    PLATFORM_BUS.with(|state| {
        let PlatformBusLifecycle::Ready { _state: state } = state else {
            return Err(MatchError::Service(crate::vm::service::Error::BadState));
        };
        let index = super::assigned::select_unique(state.assignable.iter().map(|resource| {
            if resource.profile() != profile {
                return false;
            }
            if u64::from(identity_kind) == hyper::abi::native::HYPER_NATIVE_DEVICE_IDENTITY_PCI_ID {
                return super::assigned::parse_pci_identity(identity)
                    .is_some_and(|identity| resource.pci_identity() == Some(identity));
            }
            state
                .catalogue
                .nodes
                .iter()
                .find(|device| device.id() == resource.firmware())
                .is_some_and(|device| match u64::from(identity_kind) {
                    hyper::abi::native::HYPER_NATIVE_DEVICE_IDENTITY_COMPATIBLE => {
                        device.is_compatible(identity)
                    }
                    hyper::abi::native::HYPER_NATIVE_DEVICE_IDENTITY_FDT_PATH => {
                        device.path() == identity
                    }
                    _ => false,
                })
        }))?;
        if state
            .assignable
            .iter()
            .enumerate()
            .any(|(other_index, other)| {
                other_index != index && other.claimed() && other.conflicts(&state.assignable[index])
            })
        {
            return Err(MatchError::Service(crate::vm::service::Error::Busy));
        }
        state.assignable[index]
            .claim(index)
            .ok_or(MatchError::Service(crate::vm::service::Error::Busy))
    })
}

pub(super) fn release(index: usize) {
    PLATFORM_BUS.with(|state| {
        if let PlatformBusLifecycle::Ready { _state: state } = state
            && let Some(resource) = state.assignable.get_mut(index)
        {
            resource.release();
        }
    });
}

impl InitializationReservation {
    fn acquire() -> Result<Self, Error> {
        PLATFORM_BUS.with(|lifecycle| match lifecycle {
            PlatformBusLifecycle::Empty => {
                *lifecycle = PlatformBusLifecycle::Preparing;
                Ok(Self { active: true })
            }
            PlatformBusLifecycle::Preparing => Err(Error::InitializationInProgress),
            PlatformBusLifecycle::Ready { .. } => Err(Error::AlreadyInitialized),
        })
    }

    fn commit(mut self, state: PlatformBusState) {
        let rejected = PLATFORM_BUS.with(|lifecycle| {
            if matches!(lifecycle, PlatformBusLifecycle::Preparing) {
                *lifecycle = PlatformBusLifecycle::Ready { _state: state };
                None
            } else {
                Some(state)
            }
        });
        if let Some(state) = rejected {
            // Keep all prepared devices and their permanent driver owner live
            // after releasing the platform-bus lock. Dropping that owner here
            // would enter its fail-stop path while the same lock was held.
            let _state = ManuallyDrop::new(state);
            hyper::debug::invariant_failure("device::platform_bus::commit invariant")
        }
        self.active = false;
    }
}

impl Drop for InitializationReservation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let rolled_back = PLATFORM_BUS.with(|lifecycle| {
            if matches!(lifecycle, PlatformBusLifecycle::Preparing) {
                *lifecycle = PlatformBusLifecycle::Empty;
                true
            } else {
                false
            }
        });
        if !rolled_back {
            hyper::debug::invariant_failure("device::platform_bus::drop invariant")
        }
    }
}

/// The catalogue is immutable after boot. Clone its owner under the registry
/// lock, then query/copy/allocate outside that lock.
pub(super) fn catalogue()
-> Result<hyper::mm::FallibleArc<super::firmware::Catalogue>, super::assigned::Error> {
    PLATFORM_BUS.with(|state| match state {
        PlatformBusLifecycle::Ready { _state: state } => Ok(state.catalogue.clone()),
        _ => Err(super::assigned::Error::BadState),
    })
}

pub(super) fn claim_bundle(
    entries: &[(u32, u32, u64)],
    irq_node: u32,
) -> Result<super::assigned::Claim, super::assigned::Error> {
    use super::assigned::Error;
    if entries.is_empty() || entries.len() > 8 {
        return Err(Error::InvalidArgument);
    }
    PLATFORM_BUS.with(|state| {
        let PlatformBusLifecycle::Ready { _state: state } = state else {
            return Err(Error::BadState);
        };
        let lookup = |node: u32| -> Result<usize, Error> {
            let id = state
                .catalogue
                .nodes
                .get(node as usize)
                .ok_or(Error::InvalidArgument)?
                .id();
            state
                .assignable
                .iter()
                .position(|resource| resource.profile() == 2 && resource.firmware() == id)
                .ok_or(Error::Unsupported)
        };
        let irq = lookup(irq_node)?;
        let mut selected = [(0usize, 0u32, 0u64); 8];
        for (slot, &(node, register, offset)) in selected.iter_mut().zip(entries) {
            *slot = (lookup(node)?, register, offset);
        }
        super::assigned::Resource::claim_bundle(
            &mut state.assignable,
            &selected[..entries.len()],
            irq,
        )
    })
}
