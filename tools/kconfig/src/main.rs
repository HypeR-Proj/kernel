// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use std::{collections::BTreeMap, env, error::Error, io::Write, path::PathBuf};

use hyper_kconfig::{
    Configuration, ValueKind, defaults, dependency_is_met, load_fragment, parse_schema, read_line,
    resolve, write_config,
};

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = env::args().collect();
    let Some(command) = arguments.get(1).map(String::as_str) else {
        return Err("usage: hyper-kconfig COMMAND [INPUT] [OUTPUT]".into());
    };
    let kconfig = PathBuf::from("Kconfig");
    let output = match arguments.get(3) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(".config"),
    };
    let schema = parse_schema(&kconfig)?;

    let configuration = match command {
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
            resolve(&schema, load_fragment(&input)?)?
        }
        "config" => interactive(&schema)?,
        _ => return Err(format!("unknown command: {command}").into()),
    };
    write_config(&output, &schema, &configuration)?;
    println!("configuration written to {}", output.display());
    Ok(())
}

fn interactive(schema: &hyper_kconfig::Schema) -> Result<Configuration, Box<dyn Error>> {
    println!("{}", schema.title);
    let mut values = BTreeMap::new();
    let mut current = defaults(schema);
    for symbol in &schema.symbols {
        if !dependency_is_met(symbol, &current) {
            continue;
        }
        let Some(default) = current.value(&symbol.name) else {
            return Err(format!("CONFIG_{} has no resolved value", symbol.name).into());
        };
        loop {
            print!("{} (CONFIG_{}) [{}]: ", symbol.prompt, symbol.name, default);
            std::io::stdout().flush()?;
            let input = read_line()?;
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
            match resolve(schema, candidate) {
                Ok(configuration) => {
                    current = configuration;
                    values.insert(symbol.name.clone(), value);
                    break;
                }
                Err(error) => eprintln!("invalid value: {error}"),
            }
        }
    }
    resolve(schema, values).map_err(Into::into)
}
