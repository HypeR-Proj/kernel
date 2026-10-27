// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Build-time kernel configuration generated from `.config`.

include!(concat!(env!("OUT_DIR"), "/kernel_config.rs"));
