// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-independent state for an Arm guest virtual timer.

use hyper::hal::timer::deadline_reached;

pub const CONTROL_ENABLE: u64 = 1 << 0;
pub const CONTROL_MASK: u64 = 1 << 1;
pub const CONTROL_STATUS: u64 = 1 << 2;
pub const CONTROL_WRITABLE_MASK: u64 = CONTROL_ENABLE | CONTROL_MASK;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtualTimerWake {
    None,
    PendingNow,
    Deadline(u64),
}

/// Saved CNTV state belonging to one vCPU.
#[repr(C)]
pub struct VirtualTimerState {
    offset: u64,
    compare_value: u64,
    control: u64,
}

impl VirtualTimerState {
    pub const fn empty() -> Self {
        Self {
            offset: 0,
            compare_value: 0,
            control: 0,
        }
    }

    pub const fn offset(&self) -> u64 {
        self.offset
    }

    pub const fn compare_value(&self) -> u64 {
        self.compare_value
    }

    pub const fn writable_control(&self) -> u64 {
        self.control
    }

    pub const fn enabled(&self) -> bool {
        self.control & CONTROL_ENABLE != 0
    }

    pub const fn masked(&self) -> bool {
        self.control & CONTROL_MASK != 0
    }

    pub fn set_offset(&mut self, offset: u64) {
        self.offset = offset;
    }

    pub fn set_compare_value(&mut self, compare_value: u64) {
        self.compare_value = compare_value;
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        set_bit(&mut self.control, CONTROL_ENABLE, enabled);
    }

    pub fn set_masked(&mut self, masked: bool) {
        set_bit(&mut self.control, CONTROL_MASK, masked);
    }

    pub fn restore_hardware_state(&mut self, offset: u64, compare_value: u64, control: u64) {
        self.offset = offset;
        self.compare_value = compare_value;
        self.control = control & CONTROL_WRITABLE_MASK;
    }

    pub const fn condition_met_at(&self, physical_count: u64) -> bool {
        let virtual_count = physical_count.wrapping_sub(self.offset);
        deadline_reached(virtual_count, self.compare_value)
    }

    pub const fn interrupt_asserted_at(&self, physical_count: u64) -> bool {
        self.enabled() && !self.masked() && self.condition_met_at(physical_count)
    }

    pub const fn wfi_wake_at(&self, physical_count: u64) -> VirtualTimerWake {
        if !self.enabled() || self.masked() {
            return VirtualTimerWake::None;
        }
        let deadline = self.compare_value.wrapping_add(self.offset);
        let forward_distance = deadline.wrapping_sub(physical_count);
        // Modular deadlines are ordered only within an open half-range. Treat
        // the exact half-period boundary conservatively as already pending so
        // WFI never arms an ambiguous host deadline.
        if forward_distance == 0 || forward_distance >= (1_u64 << 63) {
            VirtualTimerWake::PendingNow
        } else {
            VirtualTimerWake::Deadline(deadline)
        }
    }
}

impl Default for VirtualTimerState {
    fn default() -> Self {
        Self::empty()
    }
}

fn set_bit(value: &mut u64, bit: u64, enabled: bool) {
    if enabled {
        *value |= bit;
    } else {
        *value &= !bit;
    }
}
