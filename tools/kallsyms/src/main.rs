// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use std::cmp::Ordering;
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

const HEADER_SIZE: usize = 32;
const RECORD_SIZE: usize = 16;
const MAGIC: &[u8; 8] = b"HKALLSYM";
const VERSION: u32 = 1;

struct FunctionSymbol {
    name: String,
    address: u64,
    size: u32,
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let nm = arguments
        .next()
        .ok_or("usage: hyper-kallsyms <llvm-nm> <elf> <output>")?;
    let elf = arguments.next().ok_or("missing ELF path")?;
    let output = arguments.next().ok_or("missing output path")?;
    if arguments.next().is_some() {
        return Err("unexpected extra argument".into());
    }

    let symbols = read_symbols(Path::new(&nm), Path::new(&elf))?;
    let image = encode_symbols(&symbols)?;
    fs::write(&output, image)?;
    println!(
        "generated exact kallsyms image for {} complete function symbols at {}",
        symbols.len(),
        Path::new(&output).display()
    );
    Ok(())
}

fn read_symbols(nm: &Path, elf: &Path) -> Result<Vec<FunctionSymbol>, Box<dyn Error>> {
    let output = Command::new(nm)
        .args(["-S", "--defined-only", "--format=posix"])
        .arg(elf)
        .output()?;
    if !output.status.success() {
        return Err(
            io::Error::other(format!("{} failed for {}", nm.display(), elf.display())).into(),
        );
    }
    let text = String::from_utf8(output.stdout)?;
    let mut symbols = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_ascii_whitespace();
        let (Some(name), Some(kind), Some(address), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !matches!(kind, "t" | "T")
            || fields.next().is_some()
            || is_unstable_compiler_helper(name)
        {
            continue;
        }
        let address = u64::from_str_radix(address, 16)?;
        let size = u32::from_str_radix(size, 16)?;
        if size != 0 {
            symbols.push(FunctionSymbol {
                name: normalize_symbol_name(name).to_owned(),
                address,
                size,
            });
        }
    }
    symbols.sort_unstable_by(compare_symbols);
    symbols.dedup_by(|later, earlier| later.address == earlier.address);
    Ok(symbols)
}

/// Rejects implementation-detail helpers whose membership depends on codegen.
///
/// LLVM synthesizes Cortex erratum thunks with link-local hash-like suffixes.
/// Embedding the symbol table can change which thunks are selected, so they
/// cannot participate in the size-preserving kallsyms fixed point and are not
/// useful diagnostic API names in any case.
fn is_unstable_compiler_helper(name: &str) -> bool {
    name.starts_with("__CortexA")
}

fn normalize_symbol_name(name: &str) -> &str {
    let Some((base, suffix)) = name.rsplit_once(".llvm.") else {
        return name;
    };
    if !base.is_empty() && !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        base
    } else {
        name
    }
}

fn compare_symbols(left: &FunctionSymbol, right: &FunctionSymbol) -> Ordering {
    left.address.cmp(&right.address).then_with(|| {
        let left_mangled = rustc_demangle::try_demangle(&left.name).is_ok();
        let right_mangled = rustc_demangle::try_demangle(&right.name).is_ok();
        left_mangled
            .cmp(&right_mangled)
            .then_with(|| right.size.cmp(&left.size))
            .then_with(|| left.name.len().cmp(&right.name.len()))
    })
}

fn encode_symbols(symbols: &[FunctionSymbol]) -> Result<Vec<u8>, Box<dyn Error>> {
    let records_size = symbols
        .len()
        .checked_mul(RECORD_SIZE)
        .ok_or("symbol record size overflow")?;
    let strings_offset = HEADER_SIZE
        .checked_add(records_size)
        .ok_or("symbol table size overflow")?;
    let strings_size = symbols.iter().try_fold(0usize, |size, symbol| {
        size.checked_add(symbol.name.len() + 1)
    });
    let strings_size = strings_size.ok_or("symbol string size overflow")?;
    let used = strings_offset
        .checked_add(strings_size)
        .ok_or("symbol image size overflow")?;
    let mut image = vec![0u8; used];
    image[..8].copy_from_slice(MAGIC);
    write_u32(&mut image, 8, VERSION)?;
    write_u32(&mut image, 12, u32::try_from(symbols.len())?)?;
    write_u32(&mut image, 16, u32::try_from(strings_offset)?)?;
    write_u32(&mut image, 20, u32::try_from(strings_size)?)?;
    let mut name_offset = 0usize;
    for (index, symbol) in symbols.iter().enumerate() {
        let record = HEADER_SIZE + index * RECORD_SIZE;
        write_u64(&mut image, record, symbol.address)?;
        write_u32(&mut image, record + 8, symbol.size)?;
        write_u32(&mut image, record + 12, u32::try_from(name_offset)?)?;
        let name = strings_offset + name_offset;
        image[name..name + symbol.name.len()].copy_from_slice(symbol.name.as_bytes());
        name_offset += symbol.name.len() + 1;
    }
    Ok(image)
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) -> Result<(), Box<dyn Error>> {
    bytes
        .get_mut(offset..offset + 4)
        .ok_or("u32 output outside symbol image")?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> Result<(), Box<dyn Error>> {
    bytes
        .get_mut(offset..offset + 8)
        .ok_or("u64 output outside symbol image")?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_unstable_compiler_helper, normalize_symbol_name};

    #[test]
    fn excludes_codegen_dependent_cortex_erratum_helpers() {
        assert!(is_unstable_compiler_helper("__CortexA53843419_63004"));
        assert!(!is_unstable_compiler_helper("__aarch64_cas8_acq_rel"));
        assert!(!is_unstable_compiler_helper("CortexA_user_symbol"));
    }

    #[test]
    fn removes_an_llvm_private_numeric_suffix() {
        assert_eq!(
            normalize_symbol_name("hyper_exception_entry.llvm.6145726293629873893"),
            "hyper_exception_entry"
        );
    }

    #[test]
    fn preserves_names_without_an_exact_llvm_private_suffix() {
        for name in [
            "hyper_exception_entry",
            "hyper_exception_entry.llvm.",
            ".llvm.1234",
            "hyper_exception_entry.llvm.internal",
            "hyper_exception_entry.llvm.1234.tail",
        ] {
            assert_eq!(normalize_symbol_name(name), name);
        }
    }
}
