// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free FDT validation, walking, and generic resource decoding.
//!
//! This facade exposes one borrowed parser model to boot discovery and the
//! post-allocator platform scanner. It owns format validation and generic
//! resource decoding, but deliberately leaves device matching and binding
//! policy to architecture discovery and physical drivers.

mod blob;
mod property;
mod resources;
mod walker;

use super::PhysicalRange;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    BadAddress,
    BadMagic,
    BadStructure,
    NoMemory,
    TooDeep,
    TooLarge,
    TooManyRegions,
    TooManyCpus,
    DuplicateCpu,
    Truncated,
    UntranslatedAddress,
    UnsupportedCells,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeId(u32);

impl NodeId {
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Resources decoded without assigning device-specific meaning to them.
pub struct NodeResources<'a> {
    pub id: NodeId,
    pub enabled: bool,
    /// Invalid device resources are quarantined, not published as capabilities.
    pub resource_error: Option<Error>,
    pub registers: &'a [PhysicalRange],
    /// A bounded, translated PCI memory aperture admitted by platform discovery.
    /// The PCI bus address is kept separate from its CPU physical address.
    pub pci_memory: Option<(u64, PhysicalRange)>,
    pub interrupt_cells: &'a [u32],
}

pub use property::{CellList, Property, PropertyError, StringList};
pub use walker::{
    NodeVisitor, VisitorPair, VisitorPairError, WalkError, discover, discover_from_bytes,
    discover_from_bytes_with, discover_with,
};
