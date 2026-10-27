// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::abi::native as abi;
use {crate::object::diagnostics::DetailRecord, crate::object::diagnostics::ObjectDetails};

pub(in crate::abi::native) fn encode_object_details(value: &ObjectDetails) -> [u8; 88] {
    let (tag, words) = match value.details.record {
        DetailRecord::Empty => (abi::HYPER_NATIVE_OBJECT_DETAIL_EMPTY, [0; 8]),
        DetailRecord::Thread { tid, role, phase } => (
            abi::HYPER_NATIVE_OBJECT_DETAIL_THREAD,
            [
                tid.unwrap_or(0),
                role,
                phase,
                u64::from(tid.is_some()),
                0,
                0,
                0,
                0,
            ],
        ),
        DetailRecord::Vmar { base, length, live } => (
            abi::HYPER_NATIVE_OBJECT_DETAIL_VMAR,
            [base, length, u64::from(live), 0, 0, 0, 0, 0],
        ),
        DetailRecord::Mapping {
            base,
            length,
            permissions,
            maximum_permissions,
        } => (
            abi::HYPER_NATIVE_OBJECT_DETAIL_MAPPING,
            [base, length, permissions, maximum_permissions, 0, 0, 0, 0],
        ),
        DetailRecord::Channel {
            peer_koid,
            local_open,
            peer_open,
            queued,
            bytes,
            byte_queue,
        } => (
            abi::HYPER_NATIVE_OBJECT_DETAIL_CHANNEL,
            [
                peer_koid,
                u64::from(local_open),
                u64::from(peer_open),
                queued,
                bytes,
                u64::from(byte_queue),
                0,
                0,
            ],
        ),
        DetailRecord::Device {
            profile,
            device_id,
            pci_identity,
            irq_domain,
            interrupt,
            interrupt_count,
            state,
            resource_count,
        } => (
            abi::HYPER_NATIVE_OBJECT_DETAIL_DEVICE,
            [
                profile,
                device_id,
                pci_identity,
                irq_domain,
                interrupt,
                interrupt_count,
                state,
                resource_count,
            ],
        ),
        DetailRecord::DeviceResource {
            kind,
            base,
            length,
            offset,
            flags,
        } => (
            abi::HYPER_NATIVE_OBJECT_DETAIL_DEVICE_RESOURCE,
            [kind, base, length, offset, flags, 0, 0, 0],
        ),
    };
    let mut out = [0; 88];
    out[0..8].copy_from_slice(&value.koid.get().to_le_bytes());
    out[8..12].copy_from_slice(&value.kind.get().to_le_bytes());
    out[12..16].copy_from_slice(&(tag as u32).to_le_bytes());
    out[16..24].copy_from_slice(&value.details.next_cursor.to_le_bytes());
    for (chunk, word) in out[24..].chunks_exact_mut(8).zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    out
}
