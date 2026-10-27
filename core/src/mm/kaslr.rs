// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral KASLR offset selection.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    ImageTooLarge,
    InvalidAlignment,
    InvalidImage,
}

/// Selects an aligned image offset using the common PRNG. Seed provenance
/// remains boot policy; deterministic generation does not add entropy.
pub fn select_offset(
    seed: u64,
    image_size: u64,
    window_size: u64,
    alignment: u64,
) -> Result<u64, Error> {
    if image_size == 0 {
        return Err(Error::InvalidImage);
    }
    if !alignment.is_power_of_two() {
        return Err(Error::InvalidAlignment);
    }
    let image_span = align_up(image_size, alignment).ok_or(Error::InvalidImage)?;
    let maximum_offset = window_size
        .checked_sub(image_span)
        .ok_or(Error::ImageTooLarge)?;
    let slots = maximum_offset / alignment + 1;
    let upper = core::num::NonZeroU64::new(slots).ok_or(Error::InvalidImage)?;
    Ok(crate::random::Random::new(seed).below(upper) * alignment)
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}
