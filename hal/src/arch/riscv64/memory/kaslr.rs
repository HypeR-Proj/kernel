// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::arch::riscv64::memory::KERNEL_BASE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    ImageTooLarge,
    InvalidImage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    pub kernel_base: u64,
    pub offset: u64,
}

pub fn select(_seed: Option<u64>, image_size: u64) -> Result<Layout, Error> {
    if image_size == 0 {
        return Err(Error::InvalidImage);
    }
    KERNEL_BASE
        .checked_add(image_size)
        .filter(|end| *end <= crate::arch::riscv64::registers::SV39_LINEAR_BASE)
        .ok_or(Error::ImageTooLarge)?;
    Ok(Layout {
        kernel_base: KERNEL_BASE,
        offset: 0,
    })
}
