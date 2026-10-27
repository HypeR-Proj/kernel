// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Compact state words which reject cross-generation atomic transitions.

/// One eight-bit state branded with a 32-bit publication generation.
///
/// Packing both values into one atomic word prevents an observer delayed in
/// generation N from claiming a slot after its owner republishes generation
/// N+1. The type is inert: the surrounding protocol reserves any sentinel
/// generation and supplies the Release/Acquire edge which publishes data
/// associated with the generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GenerationTaggedState(u64);

impl GenerationTaggedState {
    pub const fn new(generation: u32, state: u8) -> Self {
        Self((generation as u64) << 8 | state as u64)
    }

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub fn state_for(self, generation: u32) -> Option<u8> {
        (self.0 >> 8 == u64::from(generation)).then_some(self.0 as u8)
    }
}
