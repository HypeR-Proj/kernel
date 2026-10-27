// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Narrow upward adapters for architecture exception and guest-exit entry.
//!
//! Raw frames and machine decoding remain below `arch`. These adapters are the
//! only paths by which architecture entry code invokes kernel crash, IRQ, and
//! VM policy, keeping the unavoidable dependency inversion explicit and small.

pub(crate) mod exception;
pub(crate) mod irq;
mod services;
pub(crate) mod user;
pub(crate) mod vmexit;

#[cfg(all(
    all(feature = "kernel-test", feature = "kernel-self-test"),
    any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)
))]
#[path = "../../tests/kernel/thread_create_affinity.rs"]
mod thread_affinity_test;
#[cfg(all(
    all(feature = "kernel-test", feature = "kernel-self-test"),
    any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)
))]
pub(crate) use thread_affinity_test::verify_thread_affinity_creation_for_test;

#[cfg(all(
    all(feature = "kernel-test", feature = "kernel-self-test"),
    any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)
))]
#[path = "../../tests/kernel/object_details.rs"]
mod object_details_test;
#[cfg(all(
    all(feature = "kernel-test", feature = "kernel-self-test"),
    any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)
))]
pub(crate) use object_details_test::verify_object_details_for_test;
