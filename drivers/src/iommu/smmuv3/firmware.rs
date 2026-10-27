// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! PCI requester routing described by the standard DT `iommu-map` binding.

use super::Error;

pub fn pci_stream_id(map: &[u8], mask: u32, requester: u16, phandle: u32) -> Result<u32, Error> {
    if map.is_empty() || !map.len().is_multiple_of(16) || phandle == 0 {
        return Err(Error::Unsupported);
    }
    let requester = u32::from(requester) & mask;
    let mut result = None;
    for entry in map.chunks_exact(16) {
        let word = |offset| {
            entry
                .get(offset..offset + 4)
                .and_then(|value| value.try_into().ok())
                .map(u32::from_be_bytes)
                .ok_or(Error::Unsupported)
        };
        let base = word(0)?;
        let target = word(4)?;
        let stream = word(8)?;
        let length = word(12)?;
        let end = base.checked_add(length).ok_or(Error::Address)?;
        if length == 0 || target == 0 || end > 65536 || stream.checked_add(length - 1).is_none() {
            return Err(Error::Address);
        }
        if requester >= base && requester < end {
            if result.is_some() || target != phandle {
                return Err(Error::Unsupported);
            }
            result = Some(stream + requester - base);
        }
    }
    result.ok_or(Error::Unsupported)
}

/// Named GIC SPI descriptors from `arm,smmu-v3`: a combined line, or separate
/// event/error lines. Optional PRI/sync descriptors are validated but unused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WiredInterrupts {
    pub event: [u32; 3],
    pub global_error: Option<[u32; 3]>,
}

pub fn wired_interrupts(names: &[u8], cells: &[u32]) -> Result<WiredInterrupts, Error> {
    if names.last() != Some(&0)
        || !cells.len().is_multiple_of(3)
        || cells.is_empty()
        || cells.len() > 12
    {
        return Err(Error::Unsupported);
    }
    let mut descriptors = [None; 5];
    let mut count = 0;
    for name in names[..names.len() - 1].split(|byte| *byte == 0) {
        let index = match name {
            b"eventq" => 0,
            b"gerror" => 1,
            b"priq" => 2,
            b"cmdq-sync" => 3,
            b"combined" => 4,
            _ => return Err(Error::Unsupported),
        };
        let descriptor: [u32; 3] = cells
            .get(count * 3..count * 3 + 3)
            .and_then(|cells| cells.try_into().ok())
            .ok_or(Error::Unsupported)?;
        // Non-inverted SPIs only; do not silently reinterpret PPI affinity or
        // another interrupt-controller ABI as a global SMMU fault source.
        if descriptor[0] != 0
            || descriptor[1] >= 988
            || !matches!(descriptor[2], 1 | 4)
            || descriptors
                .iter()
                .flatten()
                .any(|previous: &[u32; 3]| previous[1] == descriptor[1])
            || descriptors[index].is_some()
        {
            return Err(Error::Unsupported);
        }
        descriptors[index] = Some(descriptor);
        count += 1;
    }
    if count * 3 != cells.len() {
        return Err(Error::Unsupported);
    }
    if let Some(event) = descriptors[4] {
        if count != 1 {
            return Err(Error::Unsupported);
        }
        return Ok(WiredInterrupts {
            event,
            global_error: None,
        });
    }
    Ok(WiredInterrupts {
        event: descriptors[0].ok_or(Error::Unsupported)?,
        global_error: Some(descriptors[1].ok_or(Error::Unsupported)?),
    })
}
