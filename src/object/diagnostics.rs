// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pointer-free, object-local diagnostics. No payload callback may traverse a
//! process registry, acquire operational authority, or access hardware.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DetailError {
    Unsupported,
    InvalidCursor,
    Stale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Details {
    pub(crate) record: DetailRecord,
    pub(crate) next_cursor: u64,
}

impl Details {
    pub(crate) const fn last(record: DetailRecord, cursor: u64) -> Result<Self, DetailError> {
        if cursor != 0 {
            return Err(DetailError::InvalidCursor);
        }
        Ok(Self {
            record,
            next_cursor: 0,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DetailRecord {
    Empty,
    Thread {
        tid: Option<u64>,
        role: u64,
        phase: u64,
    },
    Vmar {
        base: u64,
        length: u64,
        live: bool,
    },
    Mapping {
        base: u64,
        length: u64,
        permissions: u64,
        maximum_permissions: u64,
    },
    Channel {
        peer_koid: u64,
        local_open: bool,
        peer_open: bool,
        queued: u64,
        bytes: u64,
        byte_queue: bool,
    },
    Device {
        profile: u64,
        device_id: u64,
        pci_identity: u64,
        irq_domain: u64,
        interrupt: u64,
        interrupt_count: u64,
        state: u64,
        resource_count: u64,
    },
    DeviceResource {
        kind: u64,
        base: u64,
        length: u64,
        offset: u64,
        flags: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObjectDetails {
    pub(crate) koid: super::Koid,
    pub(crate) kind: super::ObjectKind,
    pub(crate) details: Details,
}

/// Establish a weak diagnostic relation before publishing a pair of handles.
/// The stored value is a KOID, never a reference to the peer object.
pub(crate) trait PairedObject: super::UserExportableObject {
    fn bind_peer_identity(&self, peer: super::Koid);
}
