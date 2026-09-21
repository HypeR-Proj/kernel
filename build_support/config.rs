// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Shared configuration export for the kernel binary and its mechanism crates.

use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// Build scripts use validation and export; the host configurator also uses
// the interactive and serialization portions of this shared implementation.
#[allow(dead_code)]
#[path = "../tools/kconfig/src/lib.rs"]
mod kconfig;

/// Export one kernel configuration into the current Cargo package.
///
/// Cargo does not propagate build-script cfgs to dependencies. Each package
/// resolves the same kernel-root-relative input rather than its own `.config`.
pub fn export(kernel_root: &Path) -> Result<(), Box<dyn Error>> {
    let kernel_root = kernel_root.canonicalize()?;
    let configuration_path = match env::var_os("HYPER_CONFIG").filter(|path| !path.is_empty()) {
        Some(path) => kernel_root.join(path),
        None => kernel_root.join(".config"),
    };
    println!("cargo:rerun-if-env-changed=HYPER_CONFIG");
    println!(
        "cargo:rerun-if-changed={}",
        kernel_root.join("Kconfig").display()
    );
    println!("cargo:rerun-if-changed={}", configuration_path.display());
    let output_directory = PathBuf::from(env::var("OUT_DIR")?);
    let target = env::var("TARGET")?;
    export_kernel_configuration(
        &output_directory,
        &target,
        &configuration_path,
        &kernel_root,
    )
}

fn export_kernel_configuration(
    output_directory: &Path,
    target: &str,
    configuration_path: &Path,
    kernel_root: &Path,
) -> Result<(), Box<dyn Error>> {
    let (schema, configuration) =
        kconfig::load_and_validate(&kernel_root.join("Kconfig"), configuration_path).map_err(
            |error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "invalid kernel configuration {}: {error}; run `make defconfig`",
                        configuration_path.display()
                    ),
                )
            },
        )?;
    validate_architecture_configuration(&configuration, target)?;
    export_log_compile_configuration(&configuration)?;
    let mut rust_source = String::from("// Generated kernel configuration. Do not edit.\n\n");
    for symbol in &schema.symbols {
        let rust_name = format!("CONFIG_{}", symbol.name);
        let value = configuration.value(&symbol.name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{rust_name} is missing from the resolved configuration"),
            )
        })?;
        println!("cargo:rustc-env={rust_name}={value}");
        match symbol.kind {
            kconfig::ValueKind::Bool => {
                println!("cargo:rustc-check-cfg=cfg({rust_name})");
                if value == "y" {
                    println!("cargo:rustc-cfg={rust_name}");
                }
                rust_source.push_str(&format!(
                    "pub const {}: bool = {};\n",
                    symbol.name,
                    value == "y"
                ));
            }
            kconfig::ValueKind::Int => {
                println!("cargo:rustc-check-cfg=cfg({rust_name}, values({value:?}))");
                println!("cargo:rustc-cfg={rust_name}={value:?}");
                let integer = value.parse::<i64>().map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{rust_name} is not a valid integer: {error}"),
                    )
                })?;
                rust_source.push_str(&format!("pub const {}: i64 = {integer};\n", symbol.name));
            }
            kconfig::ValueKind::String => {
                println!("cargo:rustc-check-cfg=cfg({rust_name}, values({value:?}))");
                println!("cargo:rustc-cfg={rust_name}={value:?}");
                rust_source.push_str(&format!("pub const {}: &str = {value:?};\n", symbol.name));
            }
        }
    }
    fs::write(output_directory.join("kernel_config.rs"), rust_source)?;
    Ok(())
}

fn export_log_compile_configuration(
    configuration: &kconfig::Configuration,
) -> Result<(), Box<dyn Error>> {
    const LEVELS: [&str; 8] = [
        "emergency",
        "alert",
        "critical",
        "error",
        "warning",
        "notice",
        "info",
        "debug",
    ];

    let maximum = configuration
        .value("LOG_COMPILE_LEVEL")
        .ok_or("CONFIG_LOG_COMPILE_LEVEL is missing")?
        .parse::<usize>()?;
    if maximum >= LEVELS.len() {
        return Err(format!("CONFIG_LOG_COMPILE_LEVEL is outside 0..=7: {maximum}").into());
    }
    for (value, name) in LEVELS.iter().enumerate() {
        let cfg = format!("hyper_log_compile_{name}");
        println!("cargo:rustc-check-cfg=cfg({cfg})");
        if value <= maximum {
            println!("cargo:rustc-cfg={cfg}");
        }
    }
    Ok(())
}

fn validate_architecture_configuration(
    configuration: &kconfig::Configuration,
    target: &str,
) -> Result<(), Box<dyn Error>> {
    let aarch64 = configuration.value("ARCH_AARCH64") == Some("y");
    let riscv64 = configuration.value("ARCH_RISCV64") == Some("y");
    let x86_64 = configuration.value("ARCH_X86_64") == Some("y");
    if usize::from(aarch64) + usize::from(riscv64) + usize::from(x86_64) != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "exactly one architecture configuration must be enabled",
        )
        .into());
    }
    let matches_target = match target {
        "aarch64-unknown-none" => aarch64,
        "riscv64imac-unknown-none-elf" => riscv64,
        "x86_64-unknown-none" => x86_64,
        _ => true,
    };
    if !matches_target {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("kernel architecture configuration does not match target {target}"),
        )
        .into());
    }
    if aarch64 {
        validate_aarch64_address_configuration(configuration)?;
    }
    Ok(())
}

fn validate_aarch64_address_configuration(
    configuration: &kconfig::Configuration,
) -> Result<(), Box<dyn Error>> {
    let parse = |name: &str| -> Result<u32, Box<dyn Error>> {
        configuration
            .value(name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("CONFIG_{name} is missing"),
                )
            })?
            .parse::<u32>()
            .map_err(|error| format!("CONFIG_{name} is invalid: {error}").into())
    };
    let va_bits = parse("ARM64_VA_BITS")?;
    let pa_bits = parse("ARM64_PA_BITS")?;
    let ipa_bits = parse("ARM64_IPA_BITS")?;
    if !(42..=48).contains(&va_bits) {
        return Err(
            "CONFIG_ARM64_VA_BITS must be in 42..=48 for the four-level 4 KiB layout".into(),
        );
    }
    if !matches!(pa_bits, 32 | 36 | 40 | 42 | 44 | 48) {
        return Err("CONFIG_ARM64_PA_BITS must be one of 32, 36, 40, 42, 44, or 48".into());
    }
    if !(32..=39).contains(&ipa_bits) {
        return Err("CONFIG_ARM64_IPA_BITS must be in 32..=39 for the three-level root".into());
    }
    Ok(())
}
