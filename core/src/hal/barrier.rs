// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

/// Visibility domain affected by an architectural memory barrier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarrierDomain {
    NonShareable,
    InnerShareable,
    OuterShareable,
    FullSystem,
}

/// Access classes ordered by an architectural memory barrier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarrierAccess {
    Reads,
    Writes,
    All,
}

/// Architecture policy for explicit CPU barriers.
pub trait Barrier {
    fn data_memory(domain: BarrierDomain, access: BarrierAccess);
    fn data_synchronization(domain: BarrierDomain, access: BarrierAccess);
    fn instruction_synchronization();
}
