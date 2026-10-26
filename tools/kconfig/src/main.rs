// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use std::{collections::BTreeMap, env, error::Error, io::Write, path::PathBuf};

use hyper_kconfig::{
    Configuration, ValueKind, dependency_is_met, load_fragment, parse_schema, read_line, resolve,
    resolve_existing, validate_kernel_configuration, write_config,
};

mod menu;
mod protocol;

fn main() {
    if let Err(error) = run() {
        eprintln!("hyper-kconfig: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = env::args().collect();
    if arguments.get(1).map(String::as_str) == Some("--schema") {
        return protocol::run(&arguments[2..]);
    }
    let Some(command) = arguments.get(1).map(String::as_str) else {
        return Err("usage: hyper-kconfig COMMAND [INPUT] [OUTPUT]".into());
    };
    let kconfig = PathBuf::from("Kconfig");
    let output = match arguments.get(3) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(".config"),
    };
    let schema = parse_schema(&kconfig)?;

    if command == "cargo-features" {
        let [_, _, input, architecture] = arguments.as_slice() else {
            return Err("usage: hyper-kconfig cargo-features CONFIG ARCH".into());
        };
        let target = match architecture.as_str() {
            "aarch64" => "aarch64-unknown-none-softfloat",
            "riscv64" => "riscv64imac-unknown-none-elf",
            "x86_64" => "x86_64-unknown-none",
            _ => return Err(format!("unsupported kernel architecture: {architecture}").into()),
        };
        let configuration = resolve(&schema, load_fragment(&PathBuf::from(input))?)?;
        validate_kernel_configuration(&configuration, Some(target))?;
        println!("{}", configuration.kernel_cargo_features().join(" "));
        return Ok(());
    }

    let configuration = match command {
        "menuconfig" => {
            menu::run(&arguments, &output)?;
            return Ok(());
        }
        // Private frontend protocol: all parsing, visibility and validation
        // stay in the same Rust implementation used by kernel builds.
        "menuconfig-state" => {
            menu::state(&schema, &arguments)?;
            return Ok(());
        }
        "defconfig" => {
            let input = match arguments.get(2) {
                Some(path) => PathBuf::from(path),
                None => PathBuf::from("configs/qemu_aarch64_defconfig"),
            };
            resolve(&schema, load_fragment(&input)?)?
        }
        "olddefconfig" => {
            let input = match arguments.get(2) {
                Some(path) => PathBuf::from(path),
                None => output.clone(),
            };
            let (configuration, removed) = resolve_existing(&schema, load_fragment(&input)?)?;
            for name in removed {
                eprintln!(
                    "warning: removed obsolete or inactive configuration symbol CONFIG_{name}"
                );
            }
            configuration
        }
        "config" => {
            let input = arguments
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| output.clone());
            let values = if input.exists() {
                load_fragment(&input)?
            } else {
                BTreeMap::new()
            };
            interactive(&schema, values)?
        }
        _ => return Err(format!("unknown command: {command}").into()),
    };
    validate_kernel_configuration(&configuration, None)?;
    write_config(&output, &schema, &configuration)?;
    println!("configuration written to {}", output.display());
    Ok(())
}

fn migrate(
    schema: &hyper_kconfig::Schema,
    values: &mut BTreeMap<String, String>,
) -> Result<Configuration, Box<dyn Error>> {
    let (configuration, removed) = resolve_existing(schema, values.clone())?;
    for name in removed {
        values.remove(&name);
        eprintln!("warning: removed obsolete or inactive configuration symbol CONFIG_{name}");
    }
    Ok(configuration)
}

fn interactive(
    schema: &hyper_kconfig::Schema,
    mut values: BTreeMap<String, String>,
) -> Result<Configuration, Box<dyn Error>> {
    println!("{}", schema.title);
    let mut current = migrate(schema, &mut values)?;
    let mut section = "";
    for symbol in &schema.symbols {
        if !dependency_is_met(symbol, &current) {
            continue;
        }
        if symbol.section != section {
            section = &symbol.section;
            println!("\n{section}");
        }
        if let Some(index) = symbol.choice {
            let choice = &schema.choices[index];
            if choice.members.first() != Some(&symbol.name) {
                continue;
            }
            println!("{}:", choice.prompt);
            for (index, name) in choice.members.iter().enumerate() {
                let member = schema
                    .symbols
                    .iter()
                    .find(|symbol| symbol.name == *name)
                    .ok_or("choice member is missing")?;
                println!("  {}. {} ({name})", index + 1, member.prompt);
            }
            let default = choice
                .members
                .iter()
                .position(|name| current.value(name) == Some("y"))
                .ok_or("choice has no selected member")?;
            loop {
                print!("Selection [{}]: ", default + 1);
                std::io::stdout().flush()?;
                let input = read_line()?;
                let selected = if input.is_empty() {
                    Some(default)
                } else {
                    input
                        .parse::<usize>()
                        .ok()
                        .and_then(|number| number.checked_sub(1))
                };
                let Some(member) = selected.and_then(|index| choice.members.get(index)) else {
                    eprintln!("select one of 1..={}", choice.members.len());
                    continue;
                };
                for name in &choice.members {
                    values.remove(name);
                }
                values.insert(member.clone(), "y".to_owned());
                current = migrate(schema, &mut values)?;
                break;
            }
            continue;
        }
        let Some(default) = current.value(&symbol.name) else {
            return Err(format!("CONFIG_{} has no resolved value", symbol.name).into());
        };
        loop {
            print!("{} (CONFIG_{}) [{}]: ", symbol.prompt, symbol.name, default);
            std::io::stdout().flush()?;
            let input = read_line()?;
            if input == "?" {
                println!(
                    "{}",
                    if symbol.help.is_empty() {
                        "No additional help."
                    } else {
                        symbol.help.trim()
                    }
                );
                continue;
            }
            let value = if input.is_empty() && symbol.kind == ValueKind::String {
                format!("\"{default}\"")
            } else if input.is_empty() {
                default.to_owned()
            } else if symbol.kind == ValueKind::String {
                format!("\"{input}\"")
            } else {
                input
            };
            let mut candidate = values.clone();
            candidate.insert(symbol.name.clone(), value.clone());
            match migrate(schema, &mut candidate) {
                Ok(configuration) => {
                    current = configuration;
                    values = candidate;
                    break;
                }
                Err(error) => eprintln!("invalid value: {error}"),
            }
        }
    }
    resolve(schema, values).map_err(Into::into)
}
