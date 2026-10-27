// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::hal::memory::AddressTranslation;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    pub kernel_base: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    ImageTooLarge,
}

pub fn select(_seed: Option<u64>, image_size: u64) -> Result<Layout, Error> {
    let layout = crate::arch::x86_64::memory::X86_64AddressTranslation::layout();
    (image_size <= 512 * 1024 * 1024)
        .then_some(Layout {
            kernel_base: layout.kernel_base,
        })
        .ok_or(Error::ImageTooLarge)
}
