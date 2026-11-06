// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use std::{env, error::Error, fs, path::Path, process::Command};

pub fn build(output: &Path, target: &str) -> Result<(), Box<dyn Error>> {
    for source in [
        "vdso/main.rs",
        "vdso/state.rs",
        "vdso/linker.ld",
        "build_support/vdso.rs",
    ] {
        println!("cargo:rerun-if-changed={source}");
    }
    let directory = output.join("vdso");
    fs::create_dir_all(&directory)?;
    let unsupported = crate::native_abi::NATIVE_ABI
        .statuses
        .iter()
        .find(|status| status.name == "not_supported")
        .ok_or("missing NOT_SUPPORTED ABI status")?;
    fs::write(
        directory.join("abi.rs"),
        format!("const NOT_SUPPORTED: i64 = {};\n", unsupported.value),
    )?;
    let compiler = env::var("RUSTC")?;
    let object = directory.join("image.o");
    let status = Command::new(&compiler)
        .args([
            "--edition=2024",
            "--crate-type=lib",
            "--emit=obj",
            "--target",
            target,
            "-Copt-level=s",
            "-Cpanic=abort",
            "-Crelocation-model=pic",
            "-Coverflow-checks=off",
            "-Ccodegen-units=1",
            "vdso/main.rs",
            "-o",
        ])
        .arg(&object)
        .env("HYPER_VDSO_BUILD", &directory)
        .status()?;
    if !status.success() {
        return Err("vDSO Rust compilation failed".into());
    }
    let sysroot = Command::new(&compiler)
        .args(["--print", "sysroot"])
        .output()?;
    if !sysroot.status.success() {
        return Err("cannot resolve Rust sysroot".into());
    }
    let host = env::var("HOST")?;
    let linker = Path::new(std::str::from_utf8(&sysroot.stdout)?.trim())
        .join("lib/rustlib")
        .join(host)
        .join("bin/rust-lld");
    let status = Command::new(linker)
        .args([
            "-flavor",
            "gnu",
            "-shared",
            "-Bsymbolic",
            "--no-undefined",
            "--hash-style=sysv",
            "--build-id=none",
            "-z",
            "max-page-size=4096",
            "-T",
            "vdso/linker.ld",
            "-o",
        ])
        .arg(directory.join("image.so"))
        .arg(object)
        .status()?;
    if !status.success() {
        return Err("vDSO ELF link failed".into());
    }
    Ok(())
}
