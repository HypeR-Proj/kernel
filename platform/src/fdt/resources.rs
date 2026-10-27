// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral FDT node resource decoding and collection.
//!
//! `ResourceCollector` owns the fixed-depth node stack, inherited cell widths,
//! bus-range translation, and final platform region sets. It does not frame
//! structure tokens, publish visitor events, allocate, or interpret
//! binding-specific properties; those responsibilities remain below and above
//! this module respectively.

use super::super::{
    CpuInfo, CpuList, CpuListError, MAX_MEMORY_REGIONS, MAX_MMIO_REGIONS, MAX_NO_MAP_REGIONS,
    MAX_RESERVED_REGIONS, PhysicalRange, PlatformInfo, RegionList,
};
use super::{Error, NodeId, NodeResources, blob::ReservationReader, property::decode_u32};

const MAX_DEPTH: usize = 32;
// BCM2712 HDMI nodes expose nine independent register windows.
const MAX_NODE_REGIONS: usize = 16;
const MAX_BUS_RANGES: usize = 8;
const MAX_RANGE_CELLS: usize = 64;
const MAX_INTERRUPT_CELLS: usize = 16;

#[derive(Clone, Copy, Eq, PartialEq)]
enum AddressSpace {
    Physical,
    PciMemory(u32),
}

#[derive(Clone, Copy)]
struct RegisterList {
    entries: [PhysicalRange; MAX_NODE_REGIONS],
    spaces: [AddressSpace; MAX_NODE_REGIONS],
    length: usize,
}

impl RegisterList {
    const fn new() -> Self {
        Self {
            entries: [PhysicalRange::EMPTY; MAX_NODE_REGIONS],
            spaces: [AddressSpace::Physical; MAX_NODE_REGIONS],
            length: 0,
        }
    }

    fn iter(&self) -> impl Iterator<Item = (PhysicalRange, AddressSpace)> + '_ {
        self.entries[..self.length]
            .iter()
            .copied()
            .zip(self.spaces.iter().copied())
    }

    fn push(&mut self, range: PhysicalRange, space: AddressSpace) -> Result<(), Error> {
        if self.length == MAX_NODE_REGIONS {
            return Err(Error::TooManyRegions);
        }
        self.entries[self.length] = range;
        self.spaces[self.length] = space;
        self.length += 1;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct BusRange {
    child_space: AddressSpace,
    parent_space: AddressSpace,
    child_start: u64,
    parent_start: u64,
    size: u64,
}

impl BusRange {
    const EMPTY: Self = Self {
        child_space: AddressSpace::Physical,
        parent_space: AddressSpace::Physical,
        child_start: 0,
        parent_start: 0,
        size: 0,
    };
}

struct DiscoveryState {
    cpus: CpuList,
    memory: RegionList<MAX_MEMORY_REGIONS>,
    reserved: RegionList<MAX_RESERVED_REGIONS>,
    no_map: RegionList<MAX_NO_MAP_REGIONS>,
    mmio: RegionList<MAX_MMIO_REGIONS>,
}

#[derive(Clone, Copy)]
struct NodeState {
    id: NodeId,
    child_address_cells: u32,
    child_size_cells: u32,
    register_address_cells: u32,
    register_size_cells: u32,
    is_memory: bool,
    is_cpu: bool,
    is_pci: bool,
    register_pci: bool,
    pci_window: crate::pci::PciWindow,
    cpu_hardware_id: Option<u64>,
    is_reserved_region: bool,
    no_map: bool,
    children_are_reserved: bool,
    disabled: bool,
    resource_error: Option<Error>,
    ranges_error: Option<Error>,
    registers: RegisterList,
    raw_interrupts: [u32; MAX_INTERRUPT_CELLS],
    raw_interrupt_cells: usize,
    ranges_present: bool,
    ranges_supported: bool,
    ranges: [BusRange; MAX_BUS_RANGES],
    range_count: usize,
    raw_ranges: [u32; MAX_RANGE_CELLS],
    raw_range_cells: usize,
}

impl NodeState {
    const EMPTY: Self = Self {
        id: NodeId(0),
        child_address_cells: 2,
        child_size_cells: 1,
        register_address_cells: 2,
        register_size_cells: 1,
        is_memory: false,
        is_cpu: false,
        is_pci: false,
        register_pci: false,
        pci_window: crate::pci::PciWindow::EMPTY,
        cpu_hardware_id: None,
        is_reserved_region: false,
        no_map: false,
        children_are_reserved: false,
        disabled: false,
        resource_error: None,
        ranges_error: None,
        registers: RegisterList::new(),
        raw_interrupts: [0; MAX_INTERRUPT_CELLS],
        raw_interrupt_cells: 0,
        ranges_present: false,
        ranges_supported: true,
        ranges: [BusRange::EMPTY; MAX_BUS_RANGES],
        range_count: 0,
        raw_ranges: [0; MAX_RANGE_CELLS],
        raw_range_cells: 0,
    };
}

/// Owns all mutable state for one allocation-free resource walk.
pub(super) struct ResourceCollector {
    discovered: DiscoveryState,
    nodes: [NodeState; MAX_DEPTH],
    depth: usize,
    next_node_id: u32,
}

/// Owns callback scratch storage after a node leaves the collector stack.
///
/// The walker borrows this value only for `NodeVisitor::end_node`, so neither
/// translated ranges nor interrupt cells can escape the callback.
pub(super) struct CompletedNode {
    id: NodeId,
    enabled: bool,
    resource_error: Option<Error>,
    registers: [PhysicalRange; MAX_NODE_REGIONS],
    register_count: usize,
    pci_memory: Option<(u64, PhysicalRange)>,
    interrupt_cells: [u32; MAX_INTERRUPT_CELLS],
    interrupt_cell_count: usize,
}

impl CompletedNode {
    pub(super) fn as_resources(&self) -> NodeResources<'_> {
        NodeResources {
            id: self.id,
            enabled: self.enabled,
            resource_error: self.resource_error,
            registers: &self.registers[..self.register_count],
            pci_memory: self.pci_memory,
            interrupt_cells: &self.interrupt_cells[..self.interrupt_cell_count],
        }
    }
}

impl ResourceCollector {
    pub(super) fn new(reservations: ReservationReader<'_>) -> Result<Self, Error> {
        Ok(Self {
            discovered: DiscoveryState {
                cpus: CpuList::new(),
                memory: RegionList::new(),
                reserved: parse_reservations(reservations)?,
                no_map: RegionList::new(),
                mmio: RegionList::new(),
            },
            nodes: [NodeState::EMPTY; MAX_DEPTH],
            depth: 0,
            next_node_id: 0,
        })
    }

    pub(super) fn begin_node(&mut self, name: &str) -> Result<NodeId, Error> {
        begin_node(
            name,
            &mut self.depth,
            &mut self.next_node_id,
            &mut self.nodes,
        )
    }

    pub(super) fn property(&mut self, name: &str, value: &[u8]) -> Result<NodeId, Error> {
        let node = self
            .depth
            .checked_sub(1)
            .and_then(|index| self.nodes.get_mut(index))
            .ok_or(Error::BadStructure)?;
        if let Err(error) = apply_property(node, name, value) {
            if !matches!(name, "reg" | "interrupts" | "ranges") {
                return Err(error);
            }
            node.resource_error.get_or_insert(error);
            if name == "ranges" {
                node.ranges_error = Some(error);
            }
        }
        Ok(node.id)
    }

    pub(super) fn end_node(&mut self) -> Result<CompletedNode, Error> {
        end_node(&mut self.depth, &mut self.nodes, &mut self.discovered)
    }

    pub(super) fn finish(self, dtb_size: usize) -> Result<PlatformInfo, Error> {
        if self.depth != 0 {
            return Err(Error::BadStructure);
        }
        if self.discovered.memory.is_empty() {
            return Err(Error::NoMemory);
        }
        Ok(PlatformInfo {
            cpus: self.discovered.cpus,
            memory: self.discovered.memory,
            reserved: self.discovered.reserved,
            no_map: self.discovered.no_map,
            mmio: self.discovered.mmio,
            dtb_size: dtb_size as u64,
        })
    }
}

fn begin_node(
    name: &str,
    depth: &mut usize,
    next_node_id: &mut u32,
    nodes: &mut [NodeState; MAX_DEPTH],
) -> Result<NodeId, Error> {
    if *depth == MAX_DEPTH {
        return Err(Error::TooDeep);
    }
    if *depth != 0 {
        prepare_ranges(&mut nodes[*depth - 1]);
    }
    let node_id = NodeId(*next_node_id);
    *next_node_id = next_node_id.checked_add(1).ok_or(Error::TooLarge)?;
    let (address_cells, size_cells, reserved_child, parent_disabled) = if *depth == 0 {
        (2, 1, false, false)
    } else {
        let parent = nodes[*depth - 1];
        (
            parent.child_address_cells,
            parent.child_size_cells,
            parent.children_are_reserved,
            parent.disabled,
        )
    };
    nodes[*depth] = NodeState {
        id: node_id,
        register_address_cells: address_cells,
        register_size_cells: size_cells,
        register_pci: *depth != 0 && nodes[*depth - 1].is_pci,
        is_memory: name == "memory" || name.starts_with("memory@"),
        is_cpu: name == "cpu" || name.starts_with("cpu@"),
        is_reserved_region: reserved_child,
        children_are_reserved: name == "reserved-memory",
        disabled: parent_disabled,
        ..NodeState::EMPTY
    };
    *depth += 1;
    Ok(node_id)
}

fn end_node(
    depth: &mut usize,
    nodes: &mut [NodeState; MAX_DEPTH],
    discovered: &mut DiscoveryState,
) -> Result<CompletedNode, Error> {
    if *depth == 0 {
        return Err(Error::BadStructure);
    }
    *depth -= 1;
    prepare_ranges(&mut nodes[*depth]);
    finish_node(nodes[*depth], discovered, &nodes[..*depth])
}

fn finish_node(
    mut node: NodeState,
    discovered: &mut DiscoveryState,
    ancestors: &[NodeState],
) -> Result<CompletedNode, Error> {
    // A failed bus translation must not mint capabilities for descendants.
    if let Some(error) = ancestors.iter().find_map(|bus| bus.ranges_error) {
        node.resource_error.get_or_insert(error);
    }
    if let Some(error) = node.resource_error {
        if node.is_memory || node.is_reserved_region || node.is_cpu || node.children_are_reserved {
            return Err(error);
        }
        return Ok(CompletedNode {
            id: node.id,
            enabled: !node.disabled,
            resource_error: Some(error),
            registers: [PhysicalRange::EMPTY; MAX_NODE_REGIONS],
            register_count: 0,
            pci_memory: None,
            interrupt_cells: [0; MAX_INTERRUPT_CELLS],
            interrupt_cell_count: 0,
        });
    }
    let (translated_registers, translated_count) = translated_registers(&node, ancestors)?;
    let pci_memory = pci_memory_window(&node, ancestors)?;
    if !node.disabled {
        discover_node_resources(&node, discovered, ancestors)?;
        if let Some((_, range)) = pci_memory {
            insert_region(&mut discovered.mmio, range)?;
        }
    }

    Ok(CompletedNode {
        id: node.id,
        enabled: !node.disabled,
        resource_error: None,
        registers: translated_registers,
        register_count: translated_count,
        pci_memory,
        interrupt_cells: node.raw_interrupts,
        interrupt_cell_count: node.raw_interrupt_cells,
    })
}

fn translated_registers(
    node: &NodeState,
    ancestors: &[NodeState],
) -> Result<([PhysicalRange; MAX_NODE_REGIONS], usize), Error> {
    let mut translated_registers = [PhysicalRange::EMPTY; MAX_NODE_REGIONS];
    let mut translated_count = 0usize;
    for (range, space) in node.registers.iter() {
        match translate_range(range, space, ancestors) {
            Ok(range) => {
                translated_registers[translated_count] = range;
                translated_count += 1;
            }
            Err(Error::UntranslatedAddress) if !node.is_memory && !node.is_reserved_region => {}
            Err(error) => return Err(error),
        }
    }
    Ok((translated_registers, translated_count))
}

fn discover_node_resources(
    node: &NodeState,
    discovered: &mut DiscoveryState,
    ancestors: &[NodeState],
) -> Result<(), Error> {
    if node.is_cpu {
        discover_cpu(node, discovered)?;
    }

    let kind = NodeRegionKind::classify(node);
    if kind == NodeRegionKind::None {
        return Ok(());
    }
    for (range, space) in node.registers.iter() {
        let range = match translate_range(range, space, ancestors) {
            Ok(range) => range,
            Err(Error::UntranslatedAddress) if kind == NodeRegionKind::Mmio => continue,
            Err(error) => return Err(error),
        };
        match kind {
            NodeRegionKind::Memory => insert_region(&mut discovered.memory, range)?,
            NodeRegionKind::Reserved => {
                insert_region(&mut discovered.reserved, range)?;
                if node.no_map {
                    insert_region(&mut discovered.no_map, range)?;
                }
            }
            NodeRegionKind::Mmio => insert_region(&mut discovered.mmio, range)?,
            NodeRegionKind::None => {}
        }
    }
    Ok(())
}

fn discover_cpu(node: &NodeState, discovered: &mut DiscoveryState) -> Result<(), Error> {
    let hardware_id = node.cpu_hardware_id.ok_or(Error::BadStructure)?;
    discovered
        .cpus
        .push(CpuInfo { hardware_id })
        .map_err(|error| match error {
            CpuListError::Capacity => Error::TooManyCpus,
            CpuListError::Duplicate => Error::DuplicateCpu,
        })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum NodeRegionKind {
    Memory,
    Reserved,
    Mmio,
    None,
}

impl NodeRegionKind {
    const fn classify(node: &NodeState) -> Self {
        if node.is_memory {
            Self::Memory
        } else if node.is_reserved_region {
            Self::Reserved
        } else if node.is_cpu {
            Self::None
        } else {
            Self::Mmio
        }
    }
}

fn insert_region<const CAPACITY: usize>(
    regions: &mut RegionList<CAPACITY>,
    range: PhysicalRange,
) -> Result<(), Error> {
    regions.insert(range).map_err(|_| Error::TooManyRegions)
}

fn apply_property(node: &mut NodeState, name: &str, value: &[u8]) -> Result<(), Error> {
    node.pci_window.property(name, value);
    match name {
        "#address-cells" => {
            node.child_address_cells = decode_u32(value).map_err(|_| Error::BadStructure)?;
        }
        "#size-cells" => {
            node.child_size_cells = decode_u32(value).map_err(|_| Error::BadStructure)?;
        }
        "device_type" => {
            node.is_memory |= c_string_equals(value, "memory");
            node.is_cpu |= c_string_equals(value, "cpu");
            node.is_pci |= c_string_equals(value, "pci");
        }
        // The DT specification defines only "ok" and "okay" as available.
        // Treat reserved, failed, disabled, and unknown states conservatively.
        "status" => {
            node.disabled |= !(c_string_equals(value, "ok") || c_string_equals(value, "okay"));
        }
        "reg" if node.is_cpu => parse_cpu_identifier(node, value)?,
        "reg" => parse_registers(node, value)?,
        "ranges" => parse_ranges(node, value)?,
        "no-map" => node.no_map = true,
        "interrupts" => parse_interrupts(node, value)?,
        _ => {}
    }
    Ok(())
}

fn parse_cpu_identifier(node: &mut NodeState, value: &[u8]) -> Result<(), Error> {
    let address_cells = supported_cells(node.register_address_cells, false)?;
    if node.register_size_cells != 0 || value.len() != address_cells * 4 {
        return Err(Error::BadStructure);
    }
    node.cpu_hardware_id = Some(read_cells(value, address_cells)?);
    Ok(())
}

fn parse_interrupts(node: &mut NodeState, value: &[u8]) -> Result<(), Error> {
    if !value.len().is_multiple_of(4) {
        return Err(Error::Truncated);
    }
    let count = value.len() / 4;
    if count > MAX_INTERRUPT_CELLS {
        node.raw_interrupt_cells = 0;
        return Ok(());
    }
    for (index, cell) in value.chunks_exact(4).enumerate() {
        node.raw_interrupts[index] = read_u32(cell, 0)?;
    }
    node.raw_interrupt_cells = count;
    Ok(())
}

fn parse_ranges(node: &mut NodeState, value: &[u8]) -> Result<(), Error> {
    node.ranges_present = true;
    if !value.len().is_multiple_of(4) {
        return Err(Error::Truncated);
    }
    let cell_count = value.len() / 4;
    if cell_count > MAX_RANGE_CELLS {
        return Err(Error::TooManyRegions);
    }
    for (index, cell) in value.chunks_exact(4).enumerate() {
        node.raw_ranges[index] = read_u32(cell, 0)?;
    }
    node.raw_range_cells = cell_count;
    Ok(())
}

fn prepare_ranges(node: &mut NodeState) {
    if node.ranges_error.is_none()
        && let Err(error) = decode_ranges(node)
    {
        node.ranges_error = Some(error);
        node.resource_error.get_or_insert(error);
    }
    if node.ranges_error.is_some() {
        node.ranges_supported = false;
        node.range_count = 0;
    }
}

fn decode_ranges(node: &mut NodeState) -> Result<(), Error> {
    node.ranges_supported = true;
    node.range_count = 0;
    if !node.ranges_present || node.raw_range_cells == 0 {
        return Ok(());
    }

    let Ok(child_cells) = address_cells(node.child_address_cells, node.is_pci) else {
        node.ranges_supported = false;
        return Ok(());
    };
    let Ok(parent_cells) = address_cells(node.register_address_cells, node.register_pci) else {
        node.ranges_supported = false;
        return Ok(());
    };
    let Ok(size_cells) = supported_cells(node.child_size_cells, true) else {
        node.ranges_supported = false;
        return Ok(());
    };
    let tuple_cells = child_cells
        .checked_add(parent_cells)
        .and_then(|cells| cells.checked_add(size_cells))
        .ok_or(Error::BadStructure)?;
    if tuple_cells == 0 || !node.raw_range_cells.is_multiple_of(tuple_cells) {
        return Err(Error::Truncated);
    }

    for tuple in node.raw_ranges[..node.raw_range_cells].chunks_exact(tuple_cells) {
        if node.range_count == MAX_BUS_RANGES {
            return Err(Error::TooManyRegions);
        }
        let parent_offset = child_cells;
        let size_offset = child_cells + parent_cells;
        let Some((child_start, child_space)) = read_address(tuple, child_cells)? else {
            continue;
        };
        let Some((parent_start, parent_space)) =
            read_address(&tuple[parent_offset..], parent_cells)?
        else {
            continue;
        };
        let size = read_cell_words(&tuple[size_offset..], size_cells)?;
        if size == 0 {
            continue;
        }
        child_start.checked_add(size).ok_or(Error::BadStructure)?;
        parent_start.checked_add(size).ok_or(Error::BadStructure)?;
        // PCI prefetch/type bits describe windows in the same memory address
        // space, not independent aliases that may overlap ambiguously.
        if node.ranges[..node.range_count].iter().any(|prior| {
            let same_space = match (child_space, prior.child_space) {
                (AddressSpace::PciMemory(_), AddressSpace::PciMemory(_)) => true,
                (left, right) => left == right,
            };
            same_space
                && child_start < prior.child_start + prior.size
                && prior.child_start < child_start + size
        }) {
            return Err(Error::BadStructure);
        }
        node.ranges[node.range_count] = BusRange {
            child_space,
            parent_space,
            child_start,
            parent_start,
            size,
        };
        node.range_count += 1;
    }
    // A nonempty table containing only unsupported spaces is not identity.
    if node.range_count == 0 {
        node.ranges_supported = false;
    }
    Ok(())
}

fn read_cell_words(cells: &[u32], count: usize) -> Result<u64, Error> {
    let mut result = 0u64;
    for &cell in cells.get(..count).ok_or(Error::Truncated)? {
        result = result.checked_shl(32).ok_or(Error::UnsupportedCells)? | u64::from(cell);
    }
    Ok(result)
}

fn translate_range(
    mut range: PhysicalRange,
    mut space: AddressSpace,
    ancestors: &[NodeState],
) -> Result<PhysicalRange, Error> {
    // The root has no parent address space. Every lower ancestor represents a
    // bus through which the child address must be translated.
    for bus in ancestors.iter().skip(1).rev() {
        if !bus.ranges_present || !bus.ranges_supported {
            return Err(Error::UntranslatedAddress);
        }
        if bus.range_count == 0 {
            continue;
        }
        let mapping = bus.ranges[..bus.range_count]
            .iter()
            .find(|mapping| {
                mapping.child_space == space
                    && mapping.child_start <= range.start()
                    && range.end() <= mapping.child_start + mapping.size
            })
            .ok_or(Error::UntranslatedAddress)?;
        let translated = mapping
            .parent_start
            .checked_add(range.start() - mapping.child_start)
            .ok_or(Error::BadStructure)?;
        range = PhysicalRange::new(translated, range.size()).ok_or(Error::BadStructure)?;
        space = mapping.parent_space;
    }
    if space == AddressSpace::Physical {
        Ok(range)
    } else {
        Err(Error::UntranslatedAddress)
    }
}

fn address_cells(value: u32, pci: bool) -> Result<usize, Error> {
    if (1..=2).contains(&value) || (value == 3 && pci) {
        Ok(value as usize)
    } else {
        Err(Error::UnsupportedCells)
    }
}

/// PCI's first address cell is a space/type tag, not the top 32 address bits.
/// Configuration and port-I/O addresses never become CPU MMIO capabilities.
fn read_address(cells: &[u32], count: usize) -> Result<Option<(u64, AddressSpace)>, Error> {
    if count != 3 {
        return read_cell_words(cells, count).map(|value| Some((value, AddressSpace::Physical)));
    }
    let tag = *cells.first().ok_or(Error::Truncated)?;
    if !matches!((tag >> 24) & 3, 2 | 3) || tag & 0x00ff_ffff != 0 {
        return Ok(None);
    }
    read_cell_words(&cells[1..], 2).map(|value| Some((value, AddressSpace::PciMemory(tag))))
}

fn pci_memory_window(
    node: &NodeState,
    ancestors: &[NodeState],
) -> Result<Option<(u64, PhysicalRange)>, Error> {
    if node.disabled || !node.ranges_supported {
        return Ok(None);
    }
    let mut windows = node.ranges[..node.range_count].iter().filter_map(|range| {
        let AddressSpace::PciMemory(tag) = range.child_space else {
            return None;
        };
        node.pci_window
            .mapping_size(tag, range.child_start, range.size)
            .map(|size| (range, size))
    });
    let Some((window, size)) = windows.next() else {
        return Ok(None);
    };
    if windows.next().is_some() {
        return Err(Error::BadStructure);
    }
    let range = PhysicalRange::new(window.parent_start, size).ok_or(Error::BadStructure)?;
    translate_range(range, window.parent_space, ancestors)
        .map(|range| Some((window.child_start, range)))
}

fn supported_cells(value: u32, allow_zero: bool) -> Result<usize, Error> {
    let cells = usize::try_from(value).map_err(|_| Error::UnsupportedCells)?;
    if (allow_zero && cells == 0) || (1..=2).contains(&cells) {
        Ok(cells)
    } else {
        Err(Error::UnsupportedCells)
    }
}

fn parse_registers(node: &mut NodeState, value: &[u8]) -> Result<(), Error> {
    let address_cells =
        usize::try_from(node.register_address_cells).map_err(|_| Error::UnsupportedCells)?;
    let size_cells =
        usize::try_from(node.register_size_cells).map_err(|_| Error::UnsupportedCells)?;
    if self::address_cells(node.register_address_cells, node.register_pci).is_err()
        || size_cells > 2
    {
        return if node.is_memory || node.is_reserved_region {
            Err(Error::UnsupportedCells)
        } else {
            Ok(())
        };
    }
    let tuple_cells = address_cells
        .checked_add(size_cells)
        .ok_or(Error::BadStructure)?;
    let tuple_bytes = tuple_cells.checked_mul(4).ok_or(Error::BadStructure)?;
    if tuple_bytes == 0 || !value.len().is_multiple_of(tuple_bytes) {
        return Err(Error::Truncated);
    }

    for tuple in value.chunks_exact(tuple_bytes) {
        let mut words = [0; 3];
        for (word, bytes) in words
            .iter_mut()
            .zip(tuple[..address_cells * 4].chunks_exact(4))
        {
            *word = read_u32(bytes, 0)?;
        }
        let Some((start, space)) = read_address(&words, address_cells)? else {
            continue;
        };
        let size = read_cells(&tuple[address_cells * 4..], size_cells)?;
        if let Some(range) = PhysicalRange::new(start, size) {
            node.registers.push(range, space)?;
        }
    }
    Ok(())
}

fn parse_reservations(
    mut entries: ReservationReader<'_>,
) -> Result<RegionList<MAX_RESERVED_REGIONS>, Error> {
    let mut reservations = RegionList::new();
    while let Some((address, size)) = entries.next()? {
        if let Some(range) = PhysicalRange::new(address, size) {
            reservations
                .insert(range)
                .map_err(|_| Error::TooManyRegions)?;
        }
    }
    Ok(reservations)
}

fn c_string_equals(value: &[u8], expected: &str) -> bool {
    value.strip_suffix(&[0]) == Some(expected.as_bytes())
}

fn read_cells(bytes: &[u8], cells: usize) -> Result<u64, Error> {
    let mut result = 0u64;
    for index in 0..cells {
        result = (result << 32) | u64::from(read_u32(bytes, index * 4)?);
    }
    Ok(result)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let raw: [u8; 4] = bytes
        .get(offset..offset.checked_add(4).ok_or(Error::Truncated)?)
        .ok_or(Error::Truncated)?
        .try_into()
        .map_err(|_| Error::Truncated)?;
    Ok(u32::from_be_bytes(raw))
}
