// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-independent hardware interface descriptions.
//!
//! This layer contains register layouts and protocol constants only. Physical
//! drivers and virtual-device models may both depend on it, but it owns no
//! device state, probing, or kernel policy.

pub mod goldfish_rtc;
pub mod pl011;
pub mod pl031;
