// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Filesystem compatibility facade for shared budgeted buffers.
pub use hyper::collections::budgeted_string::BudgetedString;
pub use {
    hyper::collections::budgeted_vec::BudgetedIntoIter,
    hyper::collections::budgeted_vec::BudgetedVec, hyper::collections::budgeted_vec::Error,
};
