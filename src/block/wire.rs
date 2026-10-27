// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Standard virtio-scsi split-ring layout for the Native initiator.

#![cfg_attr(test, allow(dead_code))]

use hyper::abi::native as abi;

pub(crate) const MEMORY_BYTES: u64 = abi::HYPER_NATIVE_NATIVE_BLOCK_MEMORY_BYTES;
pub(crate) const QUEUE_SIZE: u16 = abi::HYPER_NATIVE_NATIVE_BLOCK_QUEUE_SIZE as u16;
pub(crate) const REQUEST_QUEUES: usize = abi::HYPER_NATIVE_NATIVE_BLOCK_QUEUE_COUNT as usize - 2;
pub(crate) const QUEUE_STRIDE: u64 = abi::HYPER_NATIVE_NATIVE_BLOCK_QUEUE_STRIDE;
pub(crate) const REQUEST_QUEUE: u64 = 8192;
pub(crate) const AVAILABLE: u64 = REQUEST_QUEUE + 256;
pub(crate) const USED: u64 = REQUEST_QUEUE + 512;
pub(crate) const REQUEST: u64 = 24576;
pub(crate) const RESPONSE: u64 = REQUEST + 128;
pub(crate) const DATA: u64 = 32768;
pub(crate) const DATA_BYTES: usize = 128 * 1024;
pub(crate) const RESPONSE_BYTES: usize = 108;

pub(crate) fn request(tag: u64, cdb: &[u8; 16]) -> [u8; 51] {
    let mut bytes = [0; 51];
    // SAM flat-space addressing: the managed backend exports TPG 1 / LUN 0.
    bytes[0] = 1;
    bytes[1] = 1;
    bytes[2] = 0x40;
    bytes[8..16].copy_from_slice(&tag.to_le_bytes());
    bytes[19..35].copy_from_slice(cdb);
    bytes
}

pub(crate) fn descriptor(address: u64, length: u32, flags: u16, next: u16) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&address.to_le_bytes());
    bytes[8..12].copy_from_slice(&length.to_le_bytes());
    bytes[12..14].copy_from_slice(&flags.to_le_bytes());
    bytes[14..].copy_from_slice(&next.to_le_bytes());
    bytes
}

pub(crate) fn transfer_cdb(write: bool, first: u64, sectors: u32) -> [u8; 16] {
    let mut cdb = [0; 16];
    cdb[0] = if write { 0x8a } else { 0x88 };
    cdb[2..10].copy_from_slice(&first.to_be_bytes());
    cdb[10..14].copy_from_slice(&sectors.to_be_bytes());
    cdb
}

pub(crate) fn capacity_cdb() -> [u8; 16] {
    let mut cdb = [0; 16];
    cdb[0] = 0x9e;
    cdb[1] = 0x10;
    cdb[13] = 32;
    cdb
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standard_scsi_addressing_and_big_endian_cdb() {
        let cdb = transfer_cdb(true, 0x1020_3040_5060_7080, 0x10203);
        assert_eq!(cdb[0], 0x8a);
        assert_eq!(&cdb[2..10], &0x1020_3040_5060_7080u64.to_be_bytes());
        assert_eq!(&cdb[10..14], &0x10203u32.to_be_bytes());
        let req = request(42, &cdb);
        assert_eq!(&req[..4], &[1, 1, 0x40, 0]);
        assert_eq!(&req[8..16], &42u64.to_le_bytes());
        assert_eq!(&req[19..35], &cdb);
    }
    #[test]
    fn queue_extents_and_data_are_disjoint() {
        for queue in 0..REQUEST_QUEUES + 2 {
            let base = queue as u64 * QUEUE_STRIDE;
            assert!(base + u64::from(QUEUE_SIZE) * 16 <= base + 256);
            assert!(base + 256 + 6 + u64::from(QUEUE_SIZE) * 2 <= base + 512);
            assert!(base + 512 + 6 + u64::from(QUEUE_SIZE) * 8 <= base + 4096);
        }
        const {
            assert!(REQUEST + 51 <= RESPONSE);
            assert!(RESPONSE + RESPONSE_BYTES as u64 <= DATA);
            assert!(REQUEST + REQUEST_QUEUES as u64 * 256 <= DATA);
            assert!(DATA + (DATA_BYTES * REQUEST_QUEUES) as u64 <= MEMORY_BYTES);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompletionError {
    Corrupt,
    Scsi,
}

/// Validate all device-written metadata before trusting successful data I/O.
/// Indices and IDs never select kernel addresses: there is one admitted head.
pub(crate) fn validate_completion(
    expected_index: u16,
    used_index: u16,
    id: u32,
    written: usize,
    response: &[u8; RESPONSE_BYTES],
    data_bytes: usize,
    read: bool,
) -> Result<(), CompletionError> {
    let writable = RESPONSE_BYTES + if read { data_bytes } else { 0 };
    if used_index != expected_index.wrapping_add(1)
        || id != 0
        || (written != 0 && written < RESPONSE_BYTES)
        || written > writable
    {
        return Err(CompletionError::Corrupt);
    }
    let sense = u32::from_le_bytes([response[0], response[1], response[2], response[3]]);
    let residual = u32::from_le_bytes([response[4], response[5], response[6], response[7]]);
    // Command responses are VIRTIO_SCSI_S_OK through S_FAILURE (0..=9).
    // TMF-only codes and the unwritten 0xff sentinel are not command completion.
    if sense > 96 || residual as usize > data_bytes || response[11] > 9 {
        return Err(CompletionError::Corrupt);
    }
    if response[10] != 0 || response[11] != 0 || residual != 0 {
        return Err(CompletionError::Scsi);
    }
    // Upstream Linux vhost-scsi publishes used.len=0 after copying the SCSI
    // response. Completion length is therefore carried by status/residual.
    if written != 0 && written != writable {
        return Err(CompletionError::Corrupt);
    }
    Ok(())
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    #[test]
    fn rejects_out_of_range_or_duplicate_used_entries() {
        let response = [0; RESPONSE_BYTES];
        assert_eq!(
            validate_completion(4, 6, 0, 620, &response, 512, true),
            Err(CompletionError::Corrupt)
        );
        assert_eq!(
            validate_completion(4, 5, 8, 620, &response, 512, true),
            Err(CompletionError::Corrupt)
        );
        assert_eq!(
            validate_completion(4, 5, 0, usize::MAX, &response, 512, true),
            Err(CompletionError::Corrupt)
        );
        assert_eq!(
            validate_completion(4, 5, 0, 107, &response, 512, true),
            Err(CompletionError::Corrupt)
        );
        assert_eq!(
            validate_completion(4, 5, 0, 619, &response, 512, true),
            Err(CompletionError::Corrupt)
        );
    }
    #[test]
    fn sequence_wrap_is_one_completion_and_writes_have_no_data_in() {
        let response = [0; RESPONSE_BYTES];
        assert_eq!(
            validate_completion(u16::MAX, 0, 0, 108, &response, 512, false),
            Ok(())
        );
        assert_eq!(
            validate_completion(u16::MAX, 0, 0, 620, &response, 512, false),
            Err(CompletionError::Corrupt)
        );
    }
    #[test]
    fn upstream_vhost_zero_used_length_uses_scsi_completion() {
        let mut response = [0; RESPONSE_BYTES];
        assert_eq!(
            validate_completion(0, 1, 0, 0, &response, 512, true),
            Ok(())
        );
        response[11] = 0xff;
        assert_eq!(
            validate_completion(0, 1, 0, 0, &response, 512, true),
            Err(CompletionError::Corrupt)
        );
    }
    #[test]
    fn scsi_errors_retire_but_malformed_responses_poison() {
        let mut response = [0; RESPONSE_BYTES];
        response[10] = 2;
        assert_eq!(
            validate_completion(0, 1, 0, 108, &response, 512, true),
            Err(CompletionError::Scsi)
        );
        response[0] = 97;
        assert_eq!(
            validate_completion(0, 1, 0, 108, &response, 512, true),
            Err(CompletionError::Corrupt)
        );
        response[0] = 0;
        response[4..8].copy_from_slice(&513u32.to_le_bytes());
        assert_eq!(
            validate_completion(0, 1, 0, 108, &response, 512, true),
            Err(CompletionError::Corrupt)
        );
    }
}

/// A completion already observed wins; readiness alone never extends a command.
/// Called before every wait, including waits woken by spurious interrupts.
pub(crate) fn command_pending(complete: bool, now: u64, deadline: u64) -> Result<bool, ()> {
    if complete {
        Ok(false)
    } else if now >= deadline {
        Err(())
    } else {
        Ok(true)
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    #[test]
    fn permanently_ready_notification_cannot_extend_pending_command() {
        // Every wait reports ready, but the used index has not advanced.
        let mut wakes = 0;
        for now in [10, 20, 29, 30, 31] {
            match command_pending(false, now, 30) {
                Ok(true) => wakes += 1,
                Err(()) => break,
                Ok(false) => panic!("spurious notification completed a request"),
            }
        }
        assert_eq!(wakes, 3);
        assert_eq!(command_pending(true, 30, 30), Ok(false));
    }
}
