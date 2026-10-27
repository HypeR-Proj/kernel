// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Immutable hierarchical RAM filesystem imported from a validated `newc` archive.
//!
//! The filesystem borrows names and payloads from the retained archive. Node
//! identifiers are assigned after sorting canonical paths, making them stable
//! for the lifetime of one mount and independent of archive entry order.

use alloc::vec::Vec;

use crate::fs::archive::cpio;

use super::{Name, NodeAttributes, NodeId, NodeKind, Path, PathComponent};

const MAXIMUM_NODES: usize = 65_536;
/// Bounds archive expansion before the single sort-and-normalize pass.
///
/// Each archive entry contributes itself and each missing-or-duplicate parent
/// candidate. Bounding this intermediate collection keeps both allocation and
/// `O(n log n)` construction CPU work independent of archive byte size.
const MAXIMUM_BUILD_CANDIDATES: usize = MAXIMUM_NODES * 4;
/// Bounds the bytes examined by candidate sorting when paths share long
/// prefixes. This counts every explicit and synthesized candidate, including
/// duplicates, because each one participates in comparison work.
const MAXIMUM_BUILD_PATH_BYTES: usize = 16 * 1024 * 1024;
const SYNTHETIC_DIRECTORY_MODE: u32 = 0o040_755;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Allocation,
    Archive(cpio::Error),
    AncestorNotDirectory,
    BuildWorkBudgetExceeded,
    DuplicatePath,
    InvalidDirectoryCookie,
    InvalidNode,
    InvalidPath,
    NotDirectory,
    NotRegularFile,
    NotSymlink,
    TooManyNodes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Node<'archive> {
    id: NodeId,
    parent: NodeId,
    path: &'archive str,
    name: &'archive str,
    data: &'archive [u8],
    attributes: NodeAttributes,
    modified_seconds: Option<u32>,
}

impl<'archive> Node<'archive> {
    pub const fn id(self) -> NodeId {
        self.id
    }

    pub const fn parent(self) -> NodeId {
        self.parent
    }

    /// Canonical path relative to the synthetic root; root itself is `/`.
    pub const fn path(self) -> &'archive str {
        self.path
    }

    /// Entry name within `parent`; the synthetic root has an empty name.
    pub const fn name(self) -> &'archive str {
        self.name
    }

    pub const fn data(self) -> &'archive [u8] {
        self.data
    }

    pub const fn mode(self) -> u32 {
        self.attributes.mode()
    }

    pub const fn kind(self) -> NodeKind {
        self.attributes.kind()
    }

    pub const fn attributes(self) -> NodeAttributes {
        self.attributes
    }

    pub const fn modified_seconds(self) -> Option<u32> {
        self.modified_seconds
    }

    pub const fn is_executable(self) -> bool {
        self.attributes.is_executable()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectoryCookie(u64);

impl DirectoryCookie {
    pub const START: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirectoryEntry<'archive> {
    node: Node<'archive>,
    next_cookie: DirectoryCookie,
}

impl<'archive> DirectoryEntry<'archive> {
    pub const fn node(self) -> Node<'archive> {
        self.node
    }

    pub const fn next_cookie(self) -> DirectoryCookie {
        self.next_cookie
    }
}

pub struct DirectoryEntries<'fs, 'archive> {
    filesystem: &'fs RamFs<'archive>,
    cursor: usize,
    end: usize,
    directory_offset: u64,
}

impl<'archive> Iterator for DirectoryEntries<'_, 'archive> {
    type Item = DirectoryEntry<'archive>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor == self.end {
            return None;
        }
        let id = *self.filesystem.children.get(self.cursor)?;
        let node = self.filesystem.node(id)?;
        self.cursor += 1;
        self.directory_offset = self.directory_offset.checked_add(1)?;
        Some(DirectoryEntry {
            node,
            next_cookie: DirectoryCookie(self.directory_offset),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.end.saturating_sub(self.cursor);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for DirectoryEntries<'_, '_> {}

struct Candidate<'archive> {
    path: &'archive str,
    data: &'archive [u8],
    mode: u32,
    kind: NodeKind,
    implicit: bool,
    modified_seconds: Option<u32>,
}

pub struct RamFs<'archive> {
    nodes: Vec<Node<'archive>>,
    children: Vec<NodeId>,
}

impl<'archive> RamFs<'archive> {
    pub fn from_newc(bytes: &'archive [u8]) -> Result<Self, Error> {
        let archive = cpio::Archive::new(bytes).map_err(Error::Archive)?;
        let mut candidates = Vec::new();
        let mut candidate_path_bytes = 0usize;
        for entry in archive.entries() {
            let entry = entry.map_err(Error::Archive)?;
            let Some(path) = canonical_archive_path(entry.name(), entry.kind())? else {
                continue;
            };
            insert_archive_entry(
                &mut candidates,
                &mut candidate_path_bytes,
                path,
                entry.data(),
                entry.mode(),
                map_kind(entry.kind()),
                entry.modified_seconds(),
            )?;
        }
        build_filesystem(normalize_candidates(candidates)?)
    }

    pub fn root(&self) -> Node<'archive> {
        match self.node(NodeId::ROOT) {
            Some(root) => root,
            None => ramfs_invariant_violation(),
        }
    }

    pub fn node(&self, id: NodeId) -> Option<Node<'archive>> {
        let index = usize::try_from(id.get()).ok()?;
        self.nodes.get(index).copied()
    }

    pub fn attributes(&self, id: NodeId) -> Result<NodeAttributes, Error> {
        self.node(id)
            .map(Node::attributes)
            .ok_or(Error::InvalidNode)
    }

    pub fn lookup_child(
        &self,
        directory: NodeId,
        name: Name<'_>,
    ) -> Result<Option<Node<'archive>>, Error> {
        let directory_node = self.node(directory).ok_or(Error::InvalidNode)?;
        if directory_node.kind() != NodeKind::Directory {
            return Err(Error::NotDirectory);
        }
        let (start, end) = self.child_range(directory);
        let children = self.children.get(start..end).ok_or(Error::InvalidNode)?;
        Ok(children
            .binary_search_by(|id| match self.node(*id) {
                Some(node) => node.name().cmp(name.as_str()),
                None => ramfs_invariant_violation(),
            })
            .ok()
            .and_then(|index| children.get(index))
            .and_then(|id| self.node(*id)))
    }

    pub fn read_at(
        &self,
        node: NodeId,
        offset: u64,
        destination: &mut [u8],
    ) -> Result<usize, Error> {
        let node = self.node(node).ok_or(Error::InvalidNode)?;
        if node.kind() != NodeKind::File {
            return Err(Error::NotRegularFile);
        }
        copy_at(node.data(), offset, destination)
    }

    pub fn read_link(&self, node: NodeId, destination: &mut [u8]) -> Result<usize, Error> {
        let node = self.node(node).ok_or(Error::InvalidNode)?;
        if node.kind() != NodeKind::Symlink {
            return Err(Error::NotSymlink);
        }
        copy_at(node.data(), 0, destination)
    }

    pub fn enumerate(
        &self,
        directory: NodeId,
        cookie: DirectoryCookie,
    ) -> Result<DirectoryEntries<'_, 'archive>, Error> {
        let directory_node = self.node(directory).ok_or(Error::InvalidNode)?;
        if directory_node.kind() != NodeKind::Directory {
            return Err(Error::NotDirectory);
        }
        let (start, end) = self.child_range(directory);
        let offset = usize::try_from(cookie.get()).map_err(|_| Error::InvalidDirectoryCookie)?;
        let cursor = start
            .checked_add(offset)
            .ok_or(Error::InvalidDirectoryCookie)?;
        if cursor > end {
            return Err(Error::InvalidDirectoryCookie);
        }
        Ok(DirectoryEntries {
            filesystem: self,
            cursor,
            end,
            directory_offset: cookie.get(),
        })
    }

    /// Temporary absolute lookup for bootstrap callers being moved to VFS.
    pub fn lookup(&self, absolute_path: &str) -> Result<Option<Node<'archive>>, Error> {
        let path = Path::new(absolute_path).map_err(|_| Error::InvalidPath)?;
        if !path.is_absolute() {
            return Err(Error::InvalidPath);
        }
        let mut current = self.root();
        for component in path.components() {
            let PathComponent::Name(name) = component else {
                return Err(Error::InvalidPath);
            };
            let Some(child) = self.lookup_child(current.id(), name)? else {
                return Ok(None);
            };
            current = child;
        }
        Ok(Some(current))
    }

    pub fn nodes(&self) -> impl ExactSizeIterator<Item = Node<'archive>> + '_ {
        self.nodes.iter().copied()
    }

    fn child_range(&self, directory: NodeId) -> (usize, usize) {
        let start = self
            .children
            .partition_point(|id| self.parent_of(*id) < directory);
        let end = self
            .children
            .partition_point(|id| self.parent_of(*id) <= directory);
        (start, end)
    }

    fn parent_of(&self, id: NodeId) -> NodeId {
        match self.node(id) {
            Some(node) => node.parent(),
            None => ramfs_invariant_violation(),
        }
    }
}

fn build_filesystem<'archive>(
    candidates: Vec<Candidate<'archive>>,
) -> Result<RamFs<'archive>, Error> {
    let total = candidates.len().checked_add(1).ok_or(Error::TooManyNodes)?;
    if total > MAXIMUM_NODES {
        return Err(Error::TooManyNodes);
    }
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(total)
        .map_err(|_| Error::Allocation)?;
    nodes.push(Node {
        id: NodeId::ROOT,
        parent: NodeId::ROOT,
        path: "/",
        name: "",
        data: &[],
        attributes: NodeAttributes::new(NodeKind::Directory, SYNTHETIC_DIRECTORY_MODE, 0),
        modified_seconds: None,
    });
    for (index, candidate) in candidates.iter().enumerate() {
        let id = node_id_for_index(index.checked_add(1).ok_or(Error::TooManyNodes)?)?;
        let (parent_path, name) = split_parent(candidate.path);
        let parent = if parent_path.is_empty() {
            NodeId::ROOT
        } else {
            let parent_index = candidates
                .binary_search_by(|candidate| candidate.path.cmp(parent_path))
                .map_err(|_| Error::InvalidPath)?;
            node_id_for_index(parent_index.checked_add(1).ok_or(Error::TooManyNodes)?)?
        };
        let size = u64::try_from(candidate.data.len()).map_err(|_| Error::InvalidPath)?;
        nodes.push(Node {
            id,
            parent,
            path: candidate.path,
            name,
            data: candidate.data,
            attributes: NodeAttributes::new(candidate.kind, candidate.mode, size),
            modified_seconds: candidate.modified_seconds,
        });
    }

    let mut children = Vec::new();
    children
        .try_reserve_exact(candidates.len())
        .map_err(|_| Error::Allocation)?;
    children.extend(nodes.iter().skip(1).map(|node| node.id()));
    children.sort_unstable_by(|left, right| {
        let left_index = match usize::try_from(left.get()) {
            Ok(index) => index,
            Err(_) => ramfs_invariant_violation(),
        };
        let left = match nodes.get(left_index) {
            Some(node) => node,
            None => ramfs_invariant_violation(),
        };
        let right_index = match usize::try_from(right.get()) {
            Ok(index) => index,
            Err(_) => ramfs_invariant_violation(),
        };
        let right = match nodes.get(right_index) {
            Some(node) => node,
            None => ramfs_invariant_violation(),
        };
        left.parent()
            .cmp(&right.parent())
            .then_with(|| left.name().cmp(right.name()))
    });
    Ok(RamFs { nodes, children })
}

fn insert_archive_entry<'archive>(
    candidates: &mut Vec<Candidate<'archive>>,
    candidate_path_bytes: &mut usize,
    path: &'archive str,
    data: &'archive [u8],
    mode: u32,
    kind: NodeKind,
    modified_seconds: u32,
) -> Result<(), Error> {
    for (separator, _) in path.match_indices('/') {
        let prefix = path.get(..separator).ok_or(Error::InvalidPath)?;
        push_candidate(
            candidates,
            candidate_path_bytes,
            Candidate::implicit_directory(prefix),
        )?;
    }
    push_candidate(
        candidates,
        candidate_path_bytes,
        Candidate {
            path,
            data,
            mode,
            kind,
            implicit: false,
            modified_seconds: Some(modified_seconds),
        },
    )
}

impl<'archive> Candidate<'archive> {
    const fn implicit_directory(path: &'archive str) -> Self {
        Self {
            path,
            data: &[],
            mode: SYNTHETIC_DIRECTORY_MODE,
            kind: NodeKind::Directory,
            implicit: true,
            modified_seconds: None,
        }
    }
}

fn push_candidate<'archive>(
    candidates: &mut Vec<Candidate<'archive>>,
    candidate_path_bytes: &mut usize,
    candidate: Candidate<'archive>,
) -> Result<(), Error> {
    if candidates.len() >= MAXIMUM_BUILD_CANDIDATES {
        return Err(Error::BuildWorkBudgetExceeded);
    }
    let next_path_bytes = candidate_path_bytes
        .checked_add(candidate.path.len())
        .ok_or(Error::BuildWorkBudgetExceeded)?;
    if next_path_bytes > MAXIMUM_BUILD_PATH_BYTES {
        return Err(Error::BuildWorkBudgetExceeded);
    }
    candidates.try_reserve(1).map_err(|_| Error::Allocation)?;
    candidates.push(candidate);
    *candidate_path_bytes = next_path_bytes;
    Ok(())
}

fn normalize_candidates<'archive>(
    mut candidates: Vec<Candidate<'archive>>,
) -> Result<Vec<Candidate<'archive>>, Error> {
    candidates.sort_unstable_by(|left, right| {
        left.path
            .cmp(right.path)
            .then_with(|| left.implicit.cmp(&right.implicit))
    });
    let mut normalized: Vec<Candidate<'archive>> = Vec::new();
    normalized
        .try_reserve(candidates.len().min(MAXIMUM_NODES))
        .map_err(|_| Error::Allocation)?;
    for candidate in candidates {
        let Some(existing) = normalized.last_mut() else {
            normalized.push(candidate);
            continue;
        };
        if existing.path != candidate.path {
            if normalized.len().checked_add(2).ok_or(Error::TooManyNodes)? > MAXIMUM_NODES {
                return Err(Error::TooManyNodes);
            }
            normalized.push(candidate);
            continue;
        }
        match (existing.implicit, candidate.implicit) {
            (true, true) | (false, true) => {
                if existing.kind != NodeKind::Directory {
                    return Err(Error::AncestorNotDirectory);
                }
            }
            (true, false) if candidate.kind == NodeKind::Directory => *existing = candidate,
            (true, false) => return Err(Error::AncestorNotDirectory),
            (false, false) => return Err(Error::DuplicatePath),
        }
    }
    Ok(normalized)
}

fn canonical_archive_path(path: &str, kind: cpio::EntryKind) -> Result<Option<&str>, Error> {
    if path == "." {
        return if kind == cpio::EntryKind::Directory {
            Ok(None)
        } else {
            Err(Error::InvalidPath)
        };
    }
    let path = match path.strip_prefix("./") {
        Some(path) => path,
        None => path,
    };
    let parsed = Path::new(path).map_err(|_| Error::InvalidPath)?;
    if parsed.is_absolute()
        || parsed
            .components()
            .any(|component| !matches!(component, PathComponent::Name(_)))
    {
        return Err(Error::InvalidPath);
    }
    Ok(Some(path))
}

fn split_parent(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}

fn node_id_for_index(index: usize) -> Result<NodeId, Error> {
    u64::try_from(index)
        .map(NodeId::from_raw)
        .map_err(|_| Error::TooManyNodes)
}

fn copy_at(source: &[u8], offset: u64, destination: &mut [u8]) -> Result<usize, Error> {
    let Ok(offset) = usize::try_from(offset) else {
        return Ok(0);
    };
    let Some(source) = source.get(offset..) else {
        return Ok(0);
    };
    let count = source.len().min(destination.len());
    let source = source.get(..count).ok_or(Error::InvalidNode)?;
    let destination = destination.get_mut(..count).ok_or(Error::InvalidNode)?;
    destination.copy_from_slice(source);
    Ok(count)
}

const fn map_kind(kind: cpio::EntryKind) -> NodeKind {
    match kind {
        cpio::EntryKind::File => NodeKind::File,
        cpio::EntryKind::Directory => NodeKind::Directory,
        cpio::EntryKind::Symlink => NodeKind::Symlink,
        cpio::EntryKind::Other => NodeKind::Other,
    }
}

#[cold]
fn ramfs_invariant_violation() -> ! {
    hyper::debug::invariant_failure("ramfs ownership invariant")
}
