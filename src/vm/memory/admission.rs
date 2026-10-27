// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Guest-origin, route-bound device memory admission and immutable extent view.

use super::super::registry::{self, VmId};

#[derive(Default)]
pub(crate) struct Reply {
    token: u64,
    frontend: u64,
    length: u64,
    count: u64,
    alias: u64,
    offset: u64,
    extent_length: u64,
    status: u64,
    query_status: u64,
}

impl Reply {
    pub(crate) const fn new() -> Self {
        Self {
            token: 0,
            frontend: 0,
            length: 0,
            count: 0,
            alias: 0,
            offset: 0,
            extent_length: 0,
            status: 1,
            query_status: 1,
        }
    }

    pub(crate) fn admit(&mut self, vm: VmId, route: u64, token: u64) {
        *self = Self::new();
        let Ok(binding) = registry::acquire_binding(vm) else {
            return;
        };
        binding.with_address_space(|space| {
            let Some(mapping) = space.live.find_mut(token) else {
                return;
            };
            if !mapping.state.admit(route) {
                return;
            }
            self.token = token;
            self.frontend = mapping.frontend;
            self.length = mapping.length;
            self.count = mapping.extents.len() as u64;
            self.status = 0;
        });
    }

    pub(crate) fn select(&mut self, vm: VmId, route: u64, index: u64) {
        self.alias = 0;
        self.offset = 0;
        self.extent_length = 0;
        self.query_status = 1;
        if self.status != 0 {
            return;
        }
        let Ok(binding) = registry::acquire_binding(vm) else {
            return;
        };
        binding.with_address_space(|space| {
            let Some(mapping) = space.live.find_mut(self.token) else {
                return;
            };
            if !mapping.state.owns(route) {
                return;
            }
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            let Some(extent) = mapping.extents.get(index) else {
                return;
            };
            self.alias = extent.alias;
            self.offset = extent.offset;
            self.extent_length = extent.length;
            self.query_status = 0;
        });
    }

    pub(crate) fn read(&self, offset: u64) -> Option<u64> {
        Some(match offset {
            0x30 => self.frontend,
            0x38 => self.length,
            0x40 => self.status,
            0x48 => self.count,
            0x58 => self.alias,
            0x60 => self.offset,
            0x68 => self.extent_length,
            0x70 => self.query_status,
            _ => return None,
        })
    }

    pub(crate) fn quiesce(&mut self, vm: VmId, route: u64, token: u64) {
        let Ok(binding) = registry::acquire_binding(vm) else {
            return;
        };
        binding.with_address_space(|space| {
            if let Some(mapping) = space.live.find_mut(token) {
                mapping.state.quiesce(route);
            }
        });
        if self.token == token {
            *self = Self::new();
        }
    }
}
