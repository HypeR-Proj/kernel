// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

pub(crate) mod kaslr;
pub(crate) mod translation_id_policy;

mod address_space;
mod layout;
mod page_table;

#[cfg(feature = "crash-console")]
pub use address_space::inspect_mapping;
pub(crate) use address_space::prepare;
pub use address_space::{
    ActivationContext, PreparedAddressSpace, SecondaryActivationContext, StackMapping,
};
pub use layout::Riscv64AddressTranslation;
pub use page_table::Error;

pub(crate) use layout::KERNEL_BASE;
