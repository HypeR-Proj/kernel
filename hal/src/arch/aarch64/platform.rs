// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::platform::{
    CpuPowerInfo, GicV2Info, GicV3Info, InterruptControllerInfo, MAX_GIC_REDISTRIBUTOR_REGIONS,
    PhysicalRange, PlatformInterrupt, PlatformInterruptTrigger, PsciCompatibleVersion, PsciConduit,
    PsciInfo, PsciInterface, PsciLegacyFunctionIds, RegionList, TimerInfo, TimerKind,
    fdt::{NodeId, NodeResources, NodeVisitor, Property},
};

const MAX_FDT_DEPTH: usize = 32;
const MAX_EARLY_CLAIMS: usize = 8;

#[derive(Clone, Copy, Debug)]
pub struct EssentialPlatformInfo {
    pub cpu_power: Option<CpuPowerInfo>,
    pub interrupt_controller: Option<InterruptControllerInfo>,
    pub timer: Option<TimerInfo>,
    claims: [Option<NodeId>; MAX_EARLY_CLAIMS],
    claim_count: usize,
}

impl EssentialPlatformInfo {
    pub fn claims(&self) -> &[Option<NodeId>] {
        &self.claims[..self.claim_count]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidDepth,
    InvalidGic,
    InvalidInterrupt,
    InvalidProperty,
    InvalidPsci,
    TooManyClaims,
}

#[derive(Clone, Copy)]
struct Candidate {
    gic_v3: bool,
    gic_v2: bool,
    cpu_offset: u32,
    timer: bool,
    psci_legacy: bool,
    psci_version: Option<PsciCompatibleVersion>,
    psci_conduit: Option<PsciConduit>,
    psci_cpu_suspend: Option<u32>,
    psci_cpu_off: Option<u32>,
    psci_cpu_on: Option<u32>,
    psci_migrate: Option<u32>,
    redistributor_regions: u32,
    redistributor_stride: Option<u64>,
}

impl Candidate {
    const EMPTY: Self = Self {
        gic_v3: false,
        gic_v2: false,
        cpu_offset: 0,
        timer: false,
        psci_legacy: false,
        psci_version: None,
        psci_conduit: None,
        psci_cpu_suspend: None,
        psci_cpu_off: None,
        psci_cpu_on: None,
        psci_migrate: None,
        redistributor_regions: 1,
        redistributor_stride: None,
    };
}

/// Allocation-free `AArch64` matching for devices required before driver probing.
pub struct EssentialDeviceDiscovery {
    nodes: [Candidate; MAX_FDT_DEPTH],
    depth: usize,
    result: EssentialPlatformInfo,
}

impl EssentialDeviceDiscovery {
    pub const fn new() -> Self {
        Self {
            nodes: [Candidate::EMPTY; MAX_FDT_DEPTH],
            depth: 0,
            result: EssentialPlatformInfo {
                cpu_power: None,
                interrupt_controller: None,
                timer: None,
                claims: [None; MAX_EARLY_CLAIMS],
                claim_count: 0,
            },
        }
    }

    pub fn finish(self) -> Result<EssentialPlatformInfo, Error> {
        if self.depth == 0 {
            Ok(self.result)
        } else {
            Err(Error::InvalidDepth)
        }
    }

    fn claim(&mut self, id: NodeId) -> Result<(), Error> {
        let slot = self
            .result
            .claims
            .get_mut(self.result.claim_count)
            .ok_or(Error::TooManyClaims)?;
        *slot = Some(id);
        self.result.claim_count += 1;
        Ok(())
    }

    fn discover_node(
        &mut self,
        node: NodeResources<'_>,
        candidate: Candidate,
    ) -> Result<(), Error> {
        if !node.enabled {
            return Ok(());
        }
        if candidate.gic_v2 && self.result.interrupt_controller.is_none() {
            if candidate.cpu_offset != 0 {
                return Err(Error::InvalidGic);
            }
            self.result.interrupt_controller = Some(InterruptControllerInfo::GicV2(GicV2Info {
                distributor: first_register(&node)?,
                cpu_interface: *node.registers.get(1).ok_or(Error::InvalidGic)?,
                hypervisor_interface: node.registers.get(2).copied(),
                virtual_cpu_interface: node.registers.get(3).copied(),
                maintenance_interrupt: discover_maintenance(&node)?,
            }));
            self.claim(node.id)?;
        }
        if candidate.gic_v3 && self.result.interrupt_controller.is_none() {
            self.result.interrupt_controller = Some(discover_gic(&node, candidate)?);
            self.claim(node.id)?;
        }
        if candidate.timer && self.result.timer.is_none() {
            self.result.timer = Some(discover_timer(&node)?);
            self.claim(node.id)?;
        }
        if (candidate.psci_version.is_some() || candidate.psci_legacy)
            && self.result.cpu_power.is_none()
        {
            self.result.cpu_power = Some(discover_psci(candidate)?);
            self.claim(node.id)?;
        }
        Ok(())
    }
}

impl Default for EssentialDeviceDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeVisitor for EssentialDeviceDiscovery {
    type Error = Error;

    fn begin_node(&mut self, _id: NodeId, _name: &str) -> Result<(), Self::Error> {
        if self.depth == MAX_FDT_DEPTH {
            return Err(Error::InvalidDepth);
        }
        self.nodes[self.depth] = Candidate::EMPTY;
        self.depth += 1;
        Ok(())
    }

    fn property(&mut self, _id: NodeId, property: Property<'_>) -> Result<(), Self::Error> {
        let candidate = self
            .depth
            .checked_sub(1)
            .and_then(|index| self.nodes.get_mut(index))
            .ok_or(Error::InvalidDepth)?;
        match property.name() {
            "compatible" => {
                candidate.gic_v2 = property
                    .contains_string("arm,gic-400")
                    .map_err(|_| Error::InvalidProperty)?
                    || property
                        .contains_string("arm,cortex-a15-gic")
                        .map_err(|_| Error::InvalidProperty)?;
                candidate.gic_v3 = property
                    .contains_string("arm,gic-v3")
                    .map_err(|_| Error::InvalidProperty)?;
                candidate.timer = property
                    .contains_string("arm,armv8-timer")
                    .map_err(|_| Error::InvalidProperty)?;
                candidate.psci_legacy = property
                    .contains_string("arm,psci")
                    .map_err(|_| Error::InvalidProperty)?;
                candidate.psci_version = if property
                    .contains_string("arm,psci-1.0")
                    .map_err(|_| Error::InvalidProperty)?
                {
                    Some(PsciCompatibleVersion::V1_0)
                } else if property
                    .contains_string("arm,psci-0.2")
                    .map_err(|_| Error::InvalidProperty)?
                {
                    Some(PsciCompatibleVersion::V0_2)
                } else {
                    None
                };
                Ok(())
            }
            "method" => {
                candidate.psci_conduit =
                    match property.string().map_err(|_| Error::InvalidProperty)? {
                        "smc" => Some(PsciConduit::Smc),
                        "hvc" => Some(PsciConduit::Hvc),
                        _ => None,
                    };
                Ok(())
            }
            "cpu_suspend" => property
                .u32()
                .map_err(|_| Error::InvalidProperty)
                .map(|value| {
                    candidate.psci_cpu_suspend = Some(value);
                }),
            "cpu_off" => property
                .u32()
                .map_err(|_| Error::InvalidProperty)
                .map(|value| {
                    candidate.psci_cpu_off = Some(value);
                }),
            "cpu_on" => property
                .u32()
                .map_err(|_| Error::InvalidProperty)
                .map(|value| {
                    candidate.psci_cpu_on = Some(value);
                }),
            "migrate" => property
                .u32()
                .map_err(|_| Error::InvalidProperty)
                .map(|value| {
                    candidate.psci_migrate = Some(value);
                }),
            "cpu-offset" => {
                candidate.cpu_offset = property.u32().map_err(|_| Error::InvalidProperty)?;
                Ok(())
            }
            "#redistributor-regions" => {
                property
                    .u32()
                    .map_err(|_| Error::InvalidProperty)
                    .map(|value| {
                        candidate.redistributor_regions = value;
                    })
            }
            "redistributor-stride" => {
                property
                    .integer()
                    .map_err(|_| Error::InvalidProperty)
                    .map(|value| {
                        candidate.redistributor_stride = Some(value);
                    })
            }
            _ => Ok(()),
        }
    }

    fn end_node(&mut self, node: NodeResources<'_>) -> Result<(), Self::Error> {
        let index = self.depth.checked_sub(1).ok_or(Error::InvalidDepth)?;
        let candidate = self.nodes[index];
        self.depth = index;
        if node.enabled
            && node.resource_error.is_some()
            && (candidate.gic_v2
                || candidate.gic_v3
                || candidate.timer
                || candidate.psci_legacy
                || candidate.psci_version.is_some())
        {
            return Err(Error::InvalidProperty);
        }
        self.discover_node(node, candidate)
    }
}

fn first_register(node: &NodeResources<'_>) -> Result<PhysicalRange, Error> {
    node.registers
        .first()
        .copied()
        .ok_or(Error::InvalidProperty)
}

fn discover_gic(
    node: &NodeResources<'_>,
    candidate: Candidate,
) -> Result<InterruptControllerInfo, Error> {
    let distributor = first_register(node)?;
    let region_count =
        usize::try_from(candidate.redistributor_regions).map_err(|_| Error::InvalidGic)?;
    if region_count == 0 || region_count > MAX_GIC_REDISTRIBUTOR_REGIONS {
        return Err(Error::InvalidGic);
    }
    let end = 1usize.checked_add(region_count).ok_or(Error::InvalidGic)?;
    let regions = node.registers.get(1..end).ok_or(Error::InvalidGic)?;
    let mut redistributors = RegionList::new();
    for &region in regions {
        redistributors
            .insert(region)
            .map_err(|_| Error::InvalidGic)?;
    }
    let maintenance_interrupt = discover_maintenance(node)?;
    Ok(InterruptControllerInfo::GicV3(GicV3Info {
        distributor,
        redistributors,
        redistributor_stride: candidate.redistributor_stride,
        maintenance_interrupt,
    }))
}

fn discover_maintenance(node: &NodeResources<'_>) -> Result<Option<PlatformInterrupt>, Error> {
    if node.interrupt_cells.is_empty() {
        return Ok(None);
    }
    let descriptor = node
        .interrupt_cells
        .get(..3)
        .ok_or(Error::InvalidInterrupt)?;
    let interrupt = decode_gic_interrupt(descriptor)?;
    if !(16..32).contains(&interrupt) {
        return Err(Error::InvalidInterrupt);
    }
    Ok(Some(PlatformInterrupt {
        interrupt,
        trigger: decode_gic_trigger(descriptor[2])?,
    }))
}

fn discover_timer(node: &NodeResources<'_>) -> Result<TimerInfo, Error> {
    let virtual_descriptor = node
        .interrupt_cells
        .get(6..9)
        .ok_or(Error::InvalidInterrupt)?;
    let hypervisor_descriptor = node
        .interrupt_cells
        .get(9..12)
        .ok_or(Error::InvalidInterrupt)?;
    Ok(TimerInfo {
        kind: TimerKind::ArmGeneric,
        virtual_timer: PlatformInterrupt {
            interrupt: decode_gic_interrupt(virtual_descriptor)?,
            trigger: decode_gic_trigger(virtual_descriptor[2])?,
        },
        hypervisor_physical: PlatformInterrupt {
            interrupt: decode_gic_interrupt(hypervisor_descriptor)?,
            trigger: decode_gic_trigger(hypervisor_descriptor[2])?,
        },
    })
}

/// Decodes one three-cell GIC interrupt specifier from a platform device.
pub fn decode_platform_interrupt(descriptor: &[u32]) -> Result<PlatformInterrupt, Error> {
    let flags = *descriptor.get(2).ok_or(Error::InvalidInterrupt)?;
    Ok(PlatformInterrupt {
        interrupt: decode_gic_interrupt(descriptor)?,
        trigger: decode_gic_trigger(flags)?,
    })
}

fn discover_psci(candidate: Candidate) -> Result<CpuPowerInfo, Error> {
    let interface = match candidate.psci_version {
        Some(version) => PsciInterface::Standard(version),
        None if candidate.psci_legacy => PsciInterface::Legacy(PsciLegacyFunctionIds {
            cpu_suspend: candidate.psci_cpu_suspend,
            cpu_off: candidate.psci_cpu_off.ok_or(Error::InvalidPsci)?,
            cpu_on: candidate.psci_cpu_on.ok_or(Error::InvalidPsci)?,
            migrate: candidate.psci_migrate,
        }),
        None => return Err(Error::InvalidPsci),
    };
    Ok(CpuPowerInfo::Psci(PsciInfo {
        conduit: candidate.psci_conduit.ok_or(Error::InvalidPsci)?,
        interface,
    }))
}

fn decode_gic_interrupt(descriptor: &[u32]) -> Result<u32, Error> {
    let interrupt_type = *descriptor.first().ok_or(Error::InvalidInterrupt)?;
    let number = *descriptor.get(1).ok_or(Error::InvalidInterrupt)?;
    match interrupt_type {
        0 => number.checked_add(32).ok_or(Error::InvalidInterrupt),
        1 if number < 16 => Ok(number + 16),
        _ => Err(Error::InvalidInterrupt),
    }
}

fn decode_gic_trigger(flags: u32) -> Result<PlatformInterruptTrigger, Error> {
    match flags & 0xf {
        1 | 2 => Ok(PlatformInterruptTrigger::Edge),
        4 | 8 => Ok(PlatformInterruptTrigger::Level),
        _ => Err(Error::InvalidInterrupt),
    }
}
