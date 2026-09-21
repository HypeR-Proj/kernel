// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "hal/src/arch/aarch64/registers.rs"]
mod aarch64_registers;
#[path = "build_support/config.rs"]
mod config;
#[allow(dead_code)]
#[path = "hal/src/arch/riscv64/registers.rs"]
mod riscv64_registers;
#[allow(dead_code)]
#[path = "hal/src/arch/x86_64/registers.rs"]
mod x86_64_registers;
type AssemblySource<'a> = (&'a str, &'a str);
type ArchitectureBuild<'a> = (&'a str, &'a [(&'a str, u64)], &'a [AssemblySource<'a>]);

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=hal/src/arch/aarch64/boot.S");
    println!("cargo:rerun-if-changed=hal/src/arch/aarch64/vectors.S");
    println!("cargo:rerun-if-changed=hal/src/arch/aarch64/context.S");
    println!("cargo:rerun-if-changed=hal/src/arch/aarch64/registers.rs");
    println!("cargo:rerun-if-changed=hal/src/arch/aarch64/linker.ld");
    println!("cargo:rerun-if-changed=hal/src/arch/riscv64/boot.S");
    println!("cargo:rerun-if-changed=hal/src/arch/riscv64/trap.S");
    println!("cargo:rerun-if-changed=hal/src/arch/riscv64/context.S");
    println!("cargo:rerun-if-changed=hal/src/arch/riscv64/guest.S");
    println!("cargo:rerun-if-changed=hal/src/arch/riscv64/user.S");
    println!("cargo:rerun-if-changed=hal/src/arch/riscv64/cache.S");
    println!("cargo:rerun-if-changed=hal/src/arch/riscv64/registers.rs");
    println!("cargo:rerun-if-changed=hal/src/arch/riscv64/linker.ld");
    println!("cargo:rerun-if-changed=hal/src/arch/x86_64/boot.S");
    println!("cargo:rerun-if-changed=hal/src/arch/x86_64/context.S");
    println!("cargo:rerun-if-changed=hal/src/arch/x86_64/vectors.S");
    println!("cargo:rerun-if-changed=hal/src/arch/x86_64/ap_trampoline.S");
    println!("cargo:rerun-if-changed=hal/src/arch/x86_64/vmx.S");
    println!("cargo:rerun-if-changed=hal/src/arch/x86_64/svm.S");
    println!("cargo:rerun-if-changed=hal/src/arch/x86_64/registers.rs");
    println!("cargo:rerun-if-changed=hal/src/arch/x86_64/linker.ld");
    println!("cargo:rerun-if-env-changed=HYPER_KALLSYMS_BLOB");
    println!("cargo:rustc-check-cfg=cfg(hyper_embed_kallsyms)");

    let output_directory = PathBuf::from(env::var("OUT_DIR")?);
    let target = env::var("TARGET")?;
    config::export(Path::new(&env::var("CARGO_MANIFEST_DIR")?))?;
    configure_kallsyms_embedding()?;

    let header_path = output_directory.join("asm_constants.h");
    let (clang_target, constants, sources): ArchitectureBuild<'_> = match target.as_str() {
        "aarch64-unknown-none" => (
            "aarch64-none-elf",
            aarch64_registers::ASM_CONSTANTS,
            &[
                ("hal/src/arch/aarch64/boot.S", "aarch64_boot.o"),
                ("hal/src/arch/aarch64/vectors.S", "aarch64_vectors.o"),
                ("hal/src/arch/aarch64/context.S", "aarch64_context.o"),
            ],
        ),
        "riscv64imac-unknown-none-elf" => (
            "riscv64-none-elf",
            riscv64_registers::ASM_CONSTANTS,
            &[
                ("hal/src/arch/riscv64/boot.S", "riscv64_boot.o"),
                ("hal/src/arch/riscv64/trap.S", "riscv64_trap.o"),
                ("hal/src/arch/riscv64/context.S", "riscv64_context.o"),
                ("hal/src/arch/riscv64/guest.S", "riscv64_guest.o"),
                ("hal/src/arch/riscv64/user.S", "riscv64_user.o"),
                ("hal/src/arch/riscv64/cache.S", "riscv64_cache.o"),
            ],
        ),
        "x86_64-unknown-none" => (
            "x86_64-none-elf",
            x86_64_registers::ASM_CONSTANTS,
            &[
                ("hal/src/arch/x86_64/boot.S", "x86_64_boot.o"),
                ("hal/src/arch/x86_64/context.S", "x86_64_context.o"),
                ("hal/src/arch/x86_64/vectors.S", "x86_64_vectors.o"),
                (
                    "hal/src/arch/x86_64/ap_trampoline.S",
                    "x86_64_ap_trampoline.o",
                ),
                ("hal/src/arch/x86_64/vmx.S", "x86_64_vmx.o"),
                ("hal/src/arch/x86_64/svm.S", "x86_64_svm.o"),
            ],
        ),
        _ => return Ok(()),
    };
    write_assembly_header(&header_path, constants)?;
    for &(source, object_name) in sources {
        let object_path = output_directory.join(object_name);
        compile_assembly(
            clang_target,
            (target == "riscv64imac-unknown-none-elf")
                .then_some("rv64imafdc_h_zicsr_zifencei_zicbom"),
            source,
            &output_directory,
            &object_path,
        )?;
        println!("cargo:rustc-link-arg-bin=hyper={}", object_path.display());
    }
    Ok(())
}

fn configure_kallsyms_embedding() -> Result<(), Box<dyn Error>> {
    let Some(path) = env::var_os("HYPER_KALLSYMS_BLOB").filter(|path| !path.is_empty()) else {
        return Ok(());
    };
    let path = PathBuf::from(path).canonicalize()?;
    if !path.is_file() {
        return Err(format!("kallsyms blob is not a regular file: {}", path.display()).into());
    }
    println!("cargo:rerun-if-changed={}", path.display());
    println!("cargo:rustc-cfg=hyper_embed_kallsyms");
    println!("cargo:rustc-env=HYPER_KALLSYMS_BLOB={}", path.display());
    Ok(())
}

fn write_assembly_header(path: &Path, constants: &[(&str, u64)]) -> io::Result<()> {
    let mut header = String::from(
        "/* Generated from the selected architecture register definitions. Do not edit. */\n\
         #ifndef HYPER_ASM_CONSTANTS_H\n\
         #define HYPER_ASM_CONSTANTS_H\n\n",
    );

    for &(name, value) in constants {
        header.push_str(&format!("#define {name} 0x{value:016x}\n"));
    }
    header.push_str("\n#endif /* HYPER_ASM_CONSTANTS_H */\n");

    fs::write(path, header)
}

fn compile_assembly(
    target: &str,
    march: Option<&str>,
    source: &str,
    include_directory: &Path,
    object_path: &Path,
) -> io::Result<()> {
    let compiler = match env::var("CLANG") {
        Ok(compiler) => compiler,
        Err(env::VarError::NotPresent) => String::from("clang"),
        Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidInput, error)),
    };
    let mut command = Command::new(&compiler);
    command.args([
        &format!("--target={target}"),
        "-ffreestanding",
        "-x",
        "assembler-with-cpp",
        "-c",
        source,
    ]);
    if let Some(march) = march {
        command.arg(format!("-march={march}"));
        command.arg("-mabi=lp64");
    }
    let status = command
        .arg("-o")
        .arg(object_path)
        .arg("-I")
        .arg(include_directory)
        .status()?;

    if !status.success() {
        return Err(io::Error::other(format!(
            "{compiler} failed to compile {source}"
        )));
    }
    Ok(())
}
