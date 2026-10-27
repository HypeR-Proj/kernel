// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free validation of `CapabilityChannel` ABI records.

pub(super) const RECORD_BYTES: usize = 24;

const _: () = assert!(
    RECORD_BYTES == core::mem::size_of::<hyper::abi::native::HyperNativeCapabilityDisposition>()
);
const _: () = assert!(
    RECORD_BYTES == core::mem::size_of::<hyper::abi::native::HyperNativeCapabilityReceiveSlot>()
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    InvalidRecord,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RightsOffer {
    Same,
    Exact(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Operation {
    Move,
    Duplicate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SendDisposition {
    pub(super) handle: u64,
    pub(super) rights: RightsOffer,
    pub(super) expected_kind: u32,
    pub(super) operation: Operation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReceiveSlot {
    pub(super) rights: u64,
    pub(super) expected_kind: u32,
}

pub(super) fn decode_send(record: &[u8]) -> Result<SendDisposition, Error> {
    if record.len() != RECORD_BYTES {
        return Err(Error::InvalidRecord);
    }
    let handle = read_u64(record, 0)?;
    if handle == 0 {
        return Err(Error::InvalidRecord);
    }
    let raw_rights = read_u64(record, 8)?;
    let rights =
        if raw_rights == hyper::abi::native::HYPER_NATIVE_CAPABILITY_DISPOSITION_SAME_RIGHTS {
            RightsOffer::Same
        } else if raw_rights & !hyper::abi::native::HYPER_NATIVE_RIGHTS_MASK == 0 {
            RightsOffer::Exact(raw_rights)
        } else {
            return Err(Error::InvalidRecord);
        };
    let expected_kind = read_u32(record, 16)?;
    if !valid_kind(expected_kind) {
        return Err(Error::InvalidRecord);
    }
    let operation = match u64::from(read_u32(record, 20)?) {
        hyper::abi::native::HYPER_NATIVE_CAPABILITY_DISPOSITION_MOVE => Operation::Move,
        hyper::abi::native::HYPER_NATIVE_CAPABILITY_DISPOSITION_DUPLICATE => Operation::Duplicate,
        _ => return Err(Error::InvalidRecord),
    };
    Ok(SendDisposition {
        handle,
        rights,
        expected_kind,
        operation,
    })
}

pub(super) fn decode_receive(record: &[u8]) -> Result<ReceiveSlot, Error> {
    if record.len() != RECORD_BYTES || read_u64(record, 0)? != 0 || read_u32(record, 20)? != 0 {
        return Err(Error::InvalidRecord);
    }
    let rights = read_u64(record, 8)?;
    if rights & !hyper::abi::native::HYPER_NATIVE_RIGHTS_MASK != 0 {
        return Err(Error::InvalidRecord);
    }
    let expected_kind = read_u32(record, 16)?;
    if !valid_kind(expected_kind) {
        return Err(Error::InvalidRecord);
    }
    Ok(ReceiveSlot {
        rights,
        expected_kind,
    })
}

#[cfg(test)]
fn has_duplicate_handles(dispositions: &[SendDisposition]) -> bool {
    dispositions.iter().enumerate().any(|(index, disposition)| {
        dispositions[..index]
            .iter()
            .any(|present| present.handle == disposition.handle)
    })
}

pub(super) fn encode_receive(
    record: &mut [u8],
    handle: u64,
    rights: u64,
    expected_kind: u32,
) -> Result<(), Error> {
    if record.len() != RECORD_BYTES
        || handle == 0
        || rights & !hyper::abi::native::HYPER_NATIVE_RIGHTS_MASK != 0
        || !valid_kind(expected_kind)
    {
        return Err(Error::InvalidRecord);
    }
    record[0..8].copy_from_slice(&handle.to_ne_bytes());
    record[8..16].copy_from_slice(&rights.to_ne_bytes());
    record[16..20].copy_from_slice(&expected_kind.to_ne_bytes());
    record[20..24].copy_from_slice(&0u32.to_ne_bytes());
    Ok(())
}

const fn valid_kind(kind: u32) -> bool {
    kind != hyper::abi::native::HYPER_NATIVE_OBJECT_NONE
        && hyper::abi::native::hyper_native_object_transfer_class(kind)
            != hyper::abi::native::HYPER_NATIVE_TRANSFER_CLASS_FORBIDDEN
}

fn read_u64(record: &[u8], offset: usize) -> Result<u64, Error> {
    let bytes: [u8; 8] = record
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(Error::InvalidRecord)?;
    Ok(u64::from_ne_bytes(bytes))
}

fn read_u32(record: &[u8], offset: usize) -> Result<u32, Error> {
    let bytes: [u8; 4] = record
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(Error::InvalidRecord)?;
    Ok(u32::from_ne_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(handle: u64, rights: u64, kind: u32, tail: u32) -> [u8; RECORD_BYTES] {
        let mut record = [0; RECORD_BYTES];
        record[0..8].copy_from_slice(&handle.to_ne_bytes());
        record[8..16].copy_from_slice(&rights.to_ne_bytes());
        record[16..20].copy_from_slice(&kind.to_ne_bytes());
        record[20..24].copy_from_slice(&tail.to_ne_bytes());
        record
    }

    #[test]
    fn receive_contract_requires_empty_output_and_known_kind() {
        let valid = record(
            0,
            hyper::abi::native::HYPER_NATIVE_RIGHT_READ,
            hyper::abi::native::HYPER_NATIVE_OBJECT_CONSOLE,
            0,
        );
        assert_eq!(
            decode_receive(&valid),
            Ok(ReceiveSlot {
                rights: hyper::abi::native::HYPER_NATIVE_RIGHT_READ,
                expected_kind: hyper::abi::native::HYPER_NATIVE_OBJECT_CONSOLE,
            })
        );
        assert_eq!(
            decode_receive(&record(1, 0, 1, 0)),
            Err(Error::InvalidRecord)
        );
        assert_eq!(
            decode_receive(&record(0, 0, 0, 0)),
            Err(Error::InvalidRecord)
        );
        assert_eq!(
            decode_receive(&record(0, 0, 1, 1)),
            Err(Error::InvalidRecord)
        );
    }

    #[test]
    fn send_disposition_distinguishes_move_duplicate_and_same_rights() {
        let move_record = record(
            7,
            hyper::abi::native::HYPER_NATIVE_CAPABILITY_DISPOSITION_SAME_RIGHTS,
            hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
            hyper::abi::native::HYPER_NATIVE_CAPABILITY_DISPOSITION_MOVE as u32,
        );
        assert_eq!(
            decode_send(&move_record),
            Ok(SendDisposition {
                handle: 7,
                rights: RightsOffer::Same,
                expected_kind: hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
                operation: Operation::Move,
            })
        );
        let duplicate = record(
            9,
            hyper::abi::native::HYPER_NATIVE_RIGHT_INSPECT,
            hyper::abi::native::HYPER_NATIVE_OBJECT_PROCESS,
            hyper::abi::native::HYPER_NATIVE_CAPABILITY_DISPOSITION_DUPLICATE as u32,
        );
        assert!(matches!(
            decode_send(&duplicate),
            Ok(SendDisposition {
                rights: RightsOffer::Exact(_),
                operation: Operation::Duplicate,
                ..
            })
        ));
    }

    #[test]
    fn output_encoding_preserves_the_validated_contract() {
        let mut output = [0xaa; RECORD_BYTES];
        assert_eq!(
            encode_receive(
                &mut output,
                17,
                hyper::abi::native::HYPER_NATIVE_RIGHT_WAIT,
                hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
            ),
            Ok(())
        );
        assert_eq!(read_u64(&output, 0), Ok(17));
        assert_eq!(
            decode_receive(&record(
                0,
                hyper::abi::native::HYPER_NATIVE_RIGHT_WAIT,
                hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
                0,
            )),
            Ok(ReceiveSlot {
                rights: hyper::abi::native::HYPER_NATIVE_RIGHT_WAIT,
                expected_kind: hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
            })
        );
        assert_eq!(&output[20..24], &[0, 0, 0, 0]);
    }

    #[test]
    fn virtual_serial_handoff_is_admitted_by_both_wire_directions() {
        let kind = hyper::abi::native::HYPER_NATIVE_OBJECT_VIRTUAL_SERIAL;
        let rights = hyper::abi::native::HYPER_NATIVE_RIGHT_TRANSFER
            | hyper::abi::native::HYPER_NATIVE_RIGHT_READ
            | hyper::abi::native::HYPER_NATIVE_RIGHT_WAIT;
        let operation = hyper::abi::native::HYPER_NATIVE_CAPABILITY_DISPOSITION_MOVE as u32;
        assert!(decode_send(&record(7, rights, kind, operation)).is_ok());
        assert!(decode_receive(&record(0, rights, kind, 0)).is_ok());
    }

    #[test]
    fn duplicate_source_handles_are_rejected_as_one_structural_batch() {
        let disposition = SendDisposition {
            handle: 3,
            rights: RightsOffer::Same,
            expected_kind: hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT,
            operation: Operation::Move,
        };
        assert!(!has_duplicate_handles(&[disposition]));
        assert!(has_duplicate_handles(&[disposition, disposition]));
    }
}
