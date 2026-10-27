// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `GICv2` software-injected list register format (no physical IRQ ownership).
use super::lr::DecodeError;
use super::{GicInterruptId, InterruptGroup, ListEntry, ListState};

pub fn encode(entry: Option<ListEntry>) -> u32 {
    let Some(entry) = entry else {
        return 0;
    };
    entry.interrupt.get()
        | if entry.interrupt.get() < 16 {
            u32::from(entry.source & 7) << 10
        } else {
            0
        }
        | (u32::from(entry.priority >> 3) << 23)
        | if entry.group == InterruptGroup::Group1 {
            1 << 30
        } else {
            0
        }
        | if entry.request_eoi_maintenance {
            1 << 19
        } else {
            0
        }
        | match entry.state {
            ListState::Pending => 1 << 28,
            ListState::Active => 2 << 28,
            ListState::PendingActive => 3 << 28,
        }
}
pub fn decode(value: u32) -> Result<Option<ListEntry>, DecodeError> {
    let state = match (value >> 28) & 3 {
        0 => return Ok(None),
        1 => ListState::Pending,
        2 => ListState::Active,
        _ => ListState::PendingActive,
    };
    if value & (1 << 31) != 0 {
        return Err(DecodeError::InvalidVirtualInterrupt);
    }
    Ok(Some(ListEntry {
        source: if value & 0x3ff < 16 {
            ((value >> 10) & 7) as u8
        } else {
            0
        },
        interrupt: GicInterruptId::new(value & 0x3ff)
            .ok_or(DecodeError::InvalidVirtualInterrupt)?,
        priority: ((value >> 23) as u8 & 31) << 3,
        group: if value & (1 << 30) != 0 {
            InterruptGroup::Group1
        } else {
            InterruptGroup::Group0
        },
        state,
        request_eoi_maintenance: value & (1 << 19) != 0,
    }))
}
