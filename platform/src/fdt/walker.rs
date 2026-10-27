// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fallible orchestration of validated FDT tokens and borrowed visitor events.
//!
//! The walker owns event ordering and error provenance. Token framing belongs
//! to `blob`, while inherited cell state and resource translation belong to
//! `resources`; binding and driver policy remain above this module.

use core::convert::Infallible;

use super::super::PlatformInfo;
use super::{
    Error, NodeId, NodeResources, Property,
    blob::{self, Blob, HEADER_SIZE, Token},
    resources::ResourceCollector,
};

/// Separates malformed FDT data from a consumer rejecting a valid event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkError<VisitorError> {
    Fdt(Error),
    Visitor(VisitorError),
}

/// Receives borrowed properties and translated resources during an FDT walk.
pub trait NodeVisitor {
    type Error;

    fn begin_node(&mut self, _id: NodeId, _name: &str) -> Result<(), Self::Error> {
        Ok(())
    }

    fn property(&mut self, _id: NodeId, _property: Property<'_>) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Observes a rejected resource property without changing error propagation.
    fn resource_error(&mut self, _property: Property<'_>, _error: Error) {}

    fn end_node(&mut self, _node: NodeResources<'_>) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Forwards one FDT walk to two independent allocation-free consumers.
pub struct VisitorPair<'a, First, Second> {
    first: &'a mut First,
    second: &'a mut Second,
}

impl<'a, First, Second> VisitorPair<'a, First, Second> {
    pub const fn new(first: &'a mut First, second: &'a mut Second) -> Self {
        Self { first, second }
    }
}

impl<First: NodeVisitor, Second: NodeVisitor> NodeVisitor for VisitorPair<'_, First, Second> {
    type Error = VisitorPairError<First::Error, Second::Error>;

    fn begin_node(&mut self, id: NodeId, name: &str) -> Result<(), Self::Error> {
        self.first
            .begin_node(id, name)
            .map_err(VisitorPairError::First)?;
        self.second
            .begin_node(id, name)
            .map_err(VisitorPairError::Second)
    }

    fn property(&mut self, id: NodeId, property: Property<'_>) -> Result<(), Self::Error> {
        self.first
            .property(id, property)
            .map_err(VisitorPairError::First)?;
        self.second
            .property(id, property)
            .map_err(VisitorPairError::Second)
    }

    fn resource_error(&mut self, property: Property<'_>, error: Error) {
        self.first.resource_error(property, error);
        self.second.resource_error(property, error);
    }

    fn end_node(&mut self, node: NodeResources<'_>) -> Result<(), Self::Error> {
        self.first
            .end_node(NodeResources {
                id: node.id,
                enabled: node.enabled,
                resource_error: node.resource_error,
                registers: node.registers,
                pci_memory: node.pci_memory,
                interrupt_cells: node.interrupt_cells,
            })
            .map_err(VisitorPairError::First)?;
        self.second.end_node(node).map_err(VisitorPairError::Second)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VisitorPairError<First, Second> {
    First(First),
    Second(Second),
}

struct IgnoreNodes;

impl NodeVisitor for IgnoreNodes {
    type Error = Infallible;
}

/// Validates and discovers platform data from a firmware-owned DTB.
///
/// # Safety
///
/// `address` must point to readable memory containing the complete DTB supplied
/// by the boot environment. The memory must remain stable during this call.
pub unsafe fn discover(address: usize) -> Result<PlatformInfo, Error> {
    // SAFETY: This function forwards its complete-DTB mapping and stability
    // contract unchanged to discover_with.
    match unsafe { discover_with(address, &mut IgnoreNodes) } {
        Ok(platform) => Ok(platform),
        Err(WalkError::Fdt(error)) => Err(error),
        Err(WalkError::Visitor(error)) => match error {},
    }
}

/// Walks the DTB while collecting architecture-independent platform memory.
///
/// Property and resource views borrow the DTB or walk-local storage and are
/// valid only for the duration of their visitor callback.
///
/// # Errors
///
/// Returns [`WalkError::Fdt`] when the blob or a generic resource encoding is
/// malformed, and [`WalkError::Visitor`] when the current valid event is
/// rejected by the consumer.
///
/// # Safety
///
/// The safety requirements are identical to [`discover`].
pub unsafe fn discover_with<V: NodeVisitor>(
    address: usize,
    visitor: &mut V,
) -> Result<PlatformInfo, WalkError<V::Error>> {
    if address == 0 || address & 0x7 != 0 || address.checked_add(HEADER_SIZE).is_none() {
        return Err(WalkError::Fdt(Error::BadAddress));
    }

    // Read only the fixed header before trusting the DTB-reported total size.
    // SAFETY: The caller guarantees at least a complete DTB; at this point we
    // expose only its fixed-size header after non-null/overflow validation.
    let header_bytes = unsafe { core::slice::from_raw_parts(address as *const u8, HEADER_SIZE) };
    let total_size = blob::total_size(header_bytes).map_err(WalkError::Fdt)?;
    if total_size > isize::MAX as usize {
        return Err(WalkError::Fdt(Error::BadAddress));
    }
    address
        .checked_add(total_size)
        .ok_or(WalkError::Fdt(Error::BadAddress))?;
    // SAFETY: The caller guarantees the complete stable blob, and total_size
    // has now passed format, address-overflow, and isize slice-size checks.
    let blob = unsafe { core::slice::from_raw_parts(address as *const u8, total_size) };
    discover_from_bytes_with(blob, visitor)
}

/// Discovers platform data in a complete, memory-backed DTB.
pub fn discover_from_bytes(blob: &[u8]) -> Result<PlatformInfo, Error> {
    match discover_from_bytes_with(blob, &mut IgnoreNodes) {
        Ok(platform) => Ok(platform),
        Err(WalkError::Fdt(error)) => Err(error),
        Err(WalkError::Visitor(error)) => match error {},
    }
}

/// Walks a memory-backed DTB with the same borrowed-event and error contract
/// as [`discover_with`].
pub fn discover_from_bytes_with<V: NodeVisitor>(
    blob: &[u8],
    visitor: &mut V,
) -> Result<PlatformInfo, WalkError<V::Error>> {
    let blob = Blob::from_bytes(blob).map_err(WalkError::Fdt)?;
    walk(&blob, visitor)
}

fn walk<V: NodeVisitor>(
    blob: &Blob<'_>,
    visitor: &mut V,
) -> Result<PlatformInfo, WalkError<V::Error>> {
    let mut resources = ResourceCollector::new(blob.reservations()).map_err(WalkError::Fdt)?;
    let mut tokens = blob.tokens();

    loop {
        match tokens.next().map_err(WalkError::Fdt)? {
            Token::BeginNode(name) => {
                let id = resources.begin_node(name).map_err(WalkError::Fdt)?;
                visitor.begin_node(id, name).map_err(WalkError::Visitor)?;
            }
            Token::EndNode => {
                let node = resources.end_node().map_err(WalkError::Fdt)?;
                visitor
                    .end_node(node.as_resources())
                    .map_err(WalkError::Visitor)?;
            }
            Token::Property { name_offset, value } => {
                let name = blob.property_name(name_offset).map_err(WalkError::Fdt)?;
                let id = resources.property(name, value).map_err(|error| {
                    visitor.resource_error(Property { name, value }, error);
                    WalkError::Fdt(error)
                })?;
                visitor
                    .property(id, Property { name, value })
                    .map_err(WalkError::Visitor)?;
            }
            Token::Nop => {}
            Token::End => return resources.finish(blob.total_size()).map_err(WalkError::Fdt),
        }
    }
}
