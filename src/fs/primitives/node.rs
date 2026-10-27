// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Backend-neutral filesystem node identity and metadata.

/// Stable identity within one filesystem instance.
///
/// The value has no meaning outside the owning filesystem instance. VFS cache
/// keys and namespace locations must therefore pair it with filesystem or
/// mount identity rather than treating it as globally unique.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeId(u64);

impl NodeId {
    pub const ROOT: Self = Self(0);

    pub const fn get(self) -> u64 {
        self.0
    }

    pub(crate) const fn from_raw(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// Owned, copyable metadata returned by filesystem adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeAttributes {
    kind: NodeKind,
    mode: u32,
    size: u64,
}

impl NodeAttributes {
    pub const fn new(kind: NodeKind, mode: u32, size: u64) -> Self {
        Self { kind, mode, size }
    }

    pub const fn kind(self) -> NodeKind {
        self.kind
    }

    pub const fn mode(self) -> u32 {
        self.mode
    }

    pub const fn size(self) -> u64 {
        self.size
    }

    pub const fn is_executable(self) -> bool {
        self.mode & 0o111 != 0
    }
}
