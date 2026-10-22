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
    validate_test_features()?;
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

/// Cargo features are additive: no test selector may implicitly grant the
/// independent permission to compile kernel test instrumentation.
fn validate_test_features() -> Result<(), Box<dyn Error>> {
    if env::var_os("CARGO_FEATURE_KERNEL_TEST").is_some() {
        return Ok(());
    }
    for feature in [
        "kernel-self-test",
        "kernel-smmuv3-test",
        "kernel-vgic-tc-test",
        "io-dma-revoke-test",
        "kernel-stack-audit",
    ] {
        let variable = format!("CARGO_FEATURE_{}", feature.replace('-', "_").to_uppercase());
        if env::var_os(variable).is_some() {
            return Err(format!(
                "kernel test feature `{feature}` requires explicit `kernel-test`; \
                 enable both only for an intentional test image"
            )
            .into());
        }
    }
    Ok(())
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
                        "invalid kernel configuration {}: {error}; use `make olddefconfig` \
                         to migrate existing settings or `make defconfig` for new defaults",
                        configuration_path.display()
                    ),
                )
            },
        )?;
    kconfig::validate_kernel_configuration(&configuration, Some(target))?;
    export_log_compile_configuration(&configuration)?;
    let mut rust_source = String::from("// Generated kernel configuration. Do not edit.\n\n");
    for symbol in &schema.symbols {
        let rust_name = format!("CONFIG_{}", symbol.name);
        if !kconfig::dependency_is_met(symbol, &configuration)
            || (symbol.choice.is_some() && configuration.value(&symbol.name) != Some("y"))
        {
            match symbol.kind {
                kconfig::ValueKind::Bool => println!("cargo:rustc-check-cfg=cfg({rust_name})"),
                _ => println!("cargo:rustc-check-cfg=cfg({rust_name}, values())"),
            }
            continue;
        }
        let value = configuration.value(&symbol.name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{rust_name} has no resolved value"),
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
