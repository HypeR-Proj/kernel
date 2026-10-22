// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Read-only generic schema protocol for product tooling. It shares the kernel
//! parser and resolver, but deliberately does not apply kernel-specific policy.

use std::{collections::BTreeMap, error::Error, path::Path};

use hyper_kconfig::{ValueKind, load_fragment, parse_schema, resolve, resolve_existing};

pub(super) fn run(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    let [schema_path, command, rest @ ..] = arguments else {
        return Err("usage: --schema KCONFIG {describe|resolve|migrate} [CONFIG]".into());
    };
    let schema = parse_schema(Path::new(schema_path))?;
    if command == "describe" && rest.is_empty() {
        let defaults = resolve(&schema, BTreeMap::new())?;
        let entries = schema.symbols.iter().map(|symbol| {
            let kind = match symbol.kind {
                ValueKind::Bool => "bool",
                ValueKind::Int => "int",
                ValueKind::String => "string",
            };
            let dependencies = symbol.dependencies.iter().map(|dependency|
                format!("{{\"name\":{},\"inverted\":{}}}", json(&format!("CONFIG_{}", dependency.symbol)), dependency.inverted)
            ).collect::<Vec<_>>().join(",");
            format!("{{\"name\":{},\"kind\":{},\"default\":{},\"prompt\":{},\"section\":{},\"help\":{},\"dependencies\":[{}]}}",
                json(&format!("CONFIG_{}", symbol.name)), json(kind),
                json(defaults.value(&symbol.name).unwrap_or("")), json(&symbol.prompt),
                json(&symbol.section), json(&symbol.help), dependencies)
        }).collect::<Vec<_>>().join(",");
        println!(
            "{{\"title\":{},\"entries\":[{}]}}",
            json(&schema.title),
            entries
        );
        return Ok(());
    }
    let [input] = rest else {
        return Err("resolve/migrate requires one input configuration".into());
    };
    let overrides = load_fragment(Path::new(input))?;
    let (configuration, removed) = match command.as_str() {
        "resolve" => (resolve(&schema, overrides.clone())?, Vec::new()),
        "migrate" => resolve_existing(&schema, overrides.clone())?,
        _ => return Err(format!("unknown schema command: {command}").into()),
    };
    let values = configuration
        .values()
        .map(|(name, value)| format!("{}:{}", json(&format!("CONFIG_{name}")), json(value)))
        .collect::<Vec<_>>()
        .join(",");
    let removed = removed
        .iter()
        .map(|name| json(&format!("CONFIG_{name}")))
        .collect::<Vec<_>>()
        .join(",");
    let assigned = overrides
        .keys()
        .map(|name| json(&format!("CONFIG_{name}")))
        .collect::<Vec<_>>()
        .join(",");
    println!("{{\"values\":{{{values}}},\"removed\":[{removed}],\"assigned\":[{assigned}]}}");
    Ok(())
}

fn json(value: &str) -> String {
    let mut result = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\u{0}'..='\u{1f}' => result.push_str(&format!("\\u{:04x}", character as u32)),
            _ => result.push(character),
        }
    }
    result.push('"');
    result
}
