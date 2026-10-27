// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `GICv3` hardware list-register encoding.

use super::{GicInterruptId, InterruptGroup, ListEntry, ListState};

const LR_VIRTUAL_ID_MASK: u64 = u32::MAX as u64;
const LR_EOI_MAINTENANCE: u64 = 1 << 41;
const LR_PRIORITY_SHIFT: u64 = 48;
const LR_GROUP1: u64 = 1 << 60;
const LR_STATE_PENDING: u64 = 1 << 62;
const LR_STATE_ACTIVE: u64 = 1 << 63;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    InvalidVirtualInterrupt,
}

pub fn encode(entry: Option<ListEntry>) -> u64 {
    let Some(entry) = entry else {
        return 0;
    };
    let mut value = u64::from(entry.interrupt.get()) & LR_VIRTUAL_ID_MASK;
    value |= u64::from(entry.priority) << LR_PRIORITY_SHIFT;
    if entry.group == InterruptGroup::Group1 {
        value |= LR_GROUP1;
    }
    if entry.request_eoi_maintenance {
        value |= LR_EOI_MAINTENANCE;
    }
    value
        | match entry.state {
            ListState::Pending => LR_STATE_PENDING,
            ListState::Active => LR_STATE_ACTIVE,
            ListState::PendingActive => LR_STATE_PENDING | LR_STATE_ACTIVE,
        }
}

pub fn decode(value: u64) -> Result<Option<ListEntry>, DecodeError> {
    let state = match value & (LR_STATE_PENDING | LR_STATE_ACTIVE) {
        0 => return Ok(None),
        LR_STATE_PENDING => ListState::Pending,
        LR_STATE_ACTIVE => ListState::Active,
        _ => ListState::PendingActive,
    };
    let interrupt = GicInterruptId::new((value & LR_VIRTUAL_ID_MASK) as u32)
        .ok_or(DecodeError::InvalidVirtualInterrupt)?;
    Ok(Some(ListEntry {
        source: 0,
        interrupt,
        priority: (value >> LR_PRIORITY_SHIFT) as u8,
        group: if value & LR_GROUP1 != 0 {
            InterruptGroup::Group1
        } else {
            InterruptGroup::Group0
        },
        state,
        request_eoi_maintenance: value & LR_EOI_MAINTENANCE != 0,
    }))
}
