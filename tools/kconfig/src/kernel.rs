// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Kernel constraints shared by configuration authoring and Cargo export.

use super::{Configuration, Error};

/// Reject values that are syntactically valid but unsupported by kernel mechanisms.
/// Host tests may select a target architecture independently of the host triple.
pub fn validate_kernel_configuration(
    configuration: &Configuration,
    target: Option<&str>,
) -> Result<(), Error> {
    let selected: Vec<_> = ["ARCH_AARCH64", "ARCH_RISCV64", "ARCH_X86_64"]
        .into_iter()
        .filter(|name| configuration.value(name) == Some("y"))
        .collect();
    let [architecture] = selected.as_slice() else {
        return Err(Error::new("exactly one architecture must be selected"));
    };
    let expected = match target {
        Some("aarch64-unknown-none-softfloat") => Some("ARCH_AARCH64"),
        Some("riscv64imac-unknown-none-elf") => Some("ARCH_RISCV64"),
        Some("x86_64-unknown-none") => Some("ARCH_X86_64"),
        _ => None,
    };
    if expected.is_some_and(|expected| expected != *architecture) {
        return Err(Error::new(format!(
            "kernel architecture CONFIG_{architecture} does not match target {target:?}"
        )));
    }
    let integer = |name: &str| -> Result<u32, Error> {
        configuration
            .value(name)
            .ok_or_else(|| Error::new(format!("CONFIG_{name} is missing")))?
            .parse()
            .map_err(|_| Error::new(format!("CONFIG_{name} must be an unsigned integer")))
    };
    if *architecture == "ARCH_AARCH64"
        && !matches!(integer("ARM64_PA_BITS")?, 32 | 36 | 40 | 42 | 44 | 48)
    {
        return Err(Error::new(
            "CONFIG_ARM64_PA_BITS must be one of 32, 36, 40, 42, 44, or 48",
        ));
    }
    for name in [
        "KERNEL_STACK_SIZE_KB",
        "IRQ_STACK_SIZE_KB",
        "EMERGENCY_STACK_SIZE_KB",
    ] {
        if !integer(name)?.is_power_of_two() {
            return Err(Error::new(format!(
                "CONFIG_{name} must be a power of two (KiB)"
            )));
        }
    }
    // Bootstrap and every possible CPU's idle thread require distinct entries.
    if integer("SCHED_MAX_THREADS")? <= integer("MAX_CPUS")? {
        return Err(Error::new(
            "CONFIG_SCHED_MAX_THREADS must exceed CONFIG_MAX_CPUS to accommodate bootstrap and per-CPU idle threads",
        ));
    }
    Ok(())
}
