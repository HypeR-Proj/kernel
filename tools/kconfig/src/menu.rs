// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Curses presentation uses this private protocol; it never interprets Kconfig.

use std::{env, error::Error, path::Path, process::Command};

use hyper_kconfig::{
    Schema, ValueKind, dependency_is_met, load_fragment, resolve, resolve_existing,
    validate_kernel_configuration, write_config,
};

pub(super) fn run(arguments: &[String], output: &Path) -> Result<(), Box<dyn Error>> {
    let input = arguments.get(2).map(Path::new).unwrap_or(output);
    let defaults = arguments
        .get(4)
        .map(Path::new)
        .unwrap_or(Path::new("configs/qemu_aarch64_defconfig"));
    let status = Command::new("python3")
        .arg("-B")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("menuconfig.py"))
        .arg(env::current_exe()?)
        .arg(input)
        .arg(output)
        .arg(defaults)
        .status()?;
    if !status.success() {
        return Err(format!("menuconfig exited with {status}").into());
    }
    Ok(())
}

/// Normalize a private working copy and return visible, validated menu entries.
/// A rejected edit leaves the working copy untouched. Never pass the user's
/// actual output here: saving is an explicit frontend commit, not an edit.
pub(super) fn state(schema: &Schema, arguments: &[String]) -> Result<(), Box<dyn Error>> {
    state_with_policy(schema, arguments, true)
}

pub(super) fn state_generic(schema: &Schema, arguments: &[String]) -> Result<(), Box<dyn Error>> {
    state_with_policy(schema, arguments, false)
}

fn state_with_policy(
    schema: &Schema,
    arguments: &[String],
    kernel_policy: bool,
) -> Result<(), Box<dyn Error>> {
    let input = arguments.get(2).ok_or("missing menu working copy")?;
    if !matches!(arguments.len(), 3 | 5) {
        return Err("usage: menuconfig-state WORKING_COPY [NAME VALUE]".into());
    }
    let mut values = load_fragment(Path::new(input))?;
    let (current, mut removed) = resolve_existing(schema, values.clone())?;
    for name in &removed {
        values.remove(name);
    }
    if let [name, value] = &arguments[3..] {
        if let Some(choice) = schema.choices.iter().find(|choice| choice.name == *name) {
            if !choice.members.contains(value) {
                return Err(format!("{value} is not a member of choice {name}").into());
            }
            for member in &choice.members {
                values.remove(member);
            }
            values.insert(value.clone(), "y".to_owned());
        } else {
            let symbol = schema
                .symbols
                .iter()
                .find(|symbol| symbol.name == *name && symbol.choice.is_none())
                .ok_or("unknown menu setting")?;
            if !dependency_is_met(symbol, &current) {
                return Err(
                    format!("CONFIG_{name} is unavailable for the selected architecture").into(),
                );
            }
            // The on-disk subset uses single-line strings, without escape
            // interpretation. Do not permit an edit to inject another key.
            if value.chars().any(char::is_control) || value.contains('"') {
                return Err("configuration values cannot contain controls or quotes".into());
            }
            values.insert(
                name.clone(),
                if symbol.kind == ValueKind::String {
                    format!("\"{value}\"")
                } else {
                    value.clone()
                },
            );
        }
    }
    // Architecture/parent changes may hide dependent settings. Drop those
    // overrides, then apply normal strict resolution and kernel constraints.
    let (_, inactive) = resolve_existing(schema, values.clone())?;
    for name in &inactive {
        values.remove(name);
    }
    removed.extend(inactive);
    let configuration = resolve(schema, values)?;
    if kernel_policy {
        validate_kernel_configuration(&configuration, None)?;
    }
    let mut entries = Vec::new();
    for symbol in &schema.symbols {
        if !dependency_is_met(symbol, &configuration) {
            continue;
        }
        let (name, prompt, kind, value, options) = if let Some(index) = symbol.choice {
            let choice = &schema.choices[index];
            if choice.members.first() != Some(&symbol.name) {
                continue;
            }
            let selected = choice
                .members
                .iter()
                .find(|name| configuration.value(name) == Some("y"))
                .ok_or("choice has no selected member")?;
            let options = choice
                .members
                .iter()
                .map(|name| {
                    let prompt = schema
                        .symbols
                        .iter()
                        .find(|member| member.name == *name)
                        .map(|member| member.prompt.as_str())
                        .unwrap_or(name);
                    format!("[{},{}]", json(name), json(prompt))
                })
                .collect::<Vec<_>>()
                .join(",");
            (
                &choice.name,
                &choice.prompt,
                "choice",
                selected.as_str(),
                options,
            )
        } else {
            let kind = match symbol.kind {
                ValueKind::Bool => "bool",
                ValueKind::Int => "int",
                ValueKind::String => "string",
            };
            let value = configuration
                .value(&symbol.name)
                .ok_or("setting has no value")?;
            (&symbol.name, &symbol.prompt, kind, value, String::new())
        };
        entries.push(format!(
            "{{\"name\":{},\"prompt\":{},\"kind\":{},\"value\":{},\"section\":{},\"help\":{},\"range\":{},\"options\":[{}]}}",
            json(name), json(prompt), json(kind), json(value), json(&symbol.section), json(&symbol.help),
            symbol.range.map(|(a, b)| format!("[{a},{b}]")).unwrap_or("null".to_owned()), options,
        ));
    }
    write_config(Path::new(input), schema, &configuration)?;
    println!(
        "{{\"title\":{},\"entries\":[{}],\"removed\":[{}]}}",
        json(&schema.title),
        entries.join(","),
        removed
            .iter()
            .map(|name| json(name))
            .collect::<Vec<_>>()
            .join(","),
    );
    Ok(())
}

/// JSON strings are the only encoding needed by this output-only protocol.
fn json(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{0}'..='\u{1f}' => output.push_str(&format!("\\u{:04x}", character as u32)),
            _ => output.push(character),
        }
    }
    output.push('"');
    output
}
