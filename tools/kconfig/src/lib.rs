// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs, io,
    path::{Path, PathBuf},
};

mod kernel;
pub use kernel::validate_kernel_configuration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValueKind {
    Bool,
    Int,
    String,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: ValueKind,
    pub prompt: String,
    pub default: String,
    pub dependencies: Vec<Dependency>,
    pub range: Option<(i64, i64)>,
    pub help: String,
    pub choice: Option<usize>,
    pub section: String,
}

/// A required, mutually exclusive boolean selection.
#[derive(Clone, Debug)]
pub struct Choice {
    pub name: String,
    pub prompt: String,
    pub default: String,
    pub members: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Dependency {
    pub symbol: String,
    pub inverted: bool,
}

#[derive(Clone, Debug)]
pub struct Schema {
    pub title: String,
    pub symbols: Vec<Symbol>,
    pub choices: Vec<Choice>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Configuration {
    values: BTreeMap<String, String>,
}

impl Configuration {
    pub fn value(&self, symbol: &str) -> Option<&str> {
        self.values.get(symbol).map(String::as_str)
    }

    pub fn values(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

#[derive(Debug)]
pub struct Error {
    path: Option<PathBuf>,
    line: Option<usize>,
    message: String,
}

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self {
            path: None,
            line: None,
            message: message.into(),
        }
    }

    fn at(path: &Path, line: usize, message: impl Into<String>) -> Self {
        Self {
            path: Some(path.to_path_buf()),
            line: Some(line),
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.path, self.line) {
            (Some(path), Some(line)) => {
                write!(formatter, "{}:{line}: {}", path.display(), self.message)
            }
            (Some(path), None) => write!(formatter, "{}: {}", path.display(), self.message),
            _ => formatter.write_str(&self.message),
        }
    }
}

impl std::error::Error for Error {}

/// Load a schema and its source files relative to the top-level Kconfig.
pub fn parse_schema(path: &Path) -> Result<Schema, Error> {
    let mut lines = Vec::new();
    let root = path.parent().unwrap_or(Path::new("."));
    read_schema_lines(path, root, &mut Vec::new(), &mut lines)?;
    parse_schema_lines(path, &lines)
}

type SourceLine = (PathBuf, usize, String);

fn read_schema_lines(
    path: &Path,
    root: &Path,
    active: &mut Vec<PathBuf>,
    lines: &mut Vec<SourceLine>,
) -> Result<(), Error> {
    let canonical =
        fs::canonicalize(path).map_err(|error| Error::at(path, 1, error.to_string()))?;
    if active.contains(&canonical) || active.len() >= 64 {
        return Err(Error::at(
            path,
            1,
            "recursive or excessively nested Kconfig source",
        ));
    }
    let source = fs::read_to_string(path).map_err(|error| Error::at(path, 1, error.to_string()))?;
    active.push(canonical);
    let mut help_indent = None;
    for (index, line) in source.lines().enumerate() {
        let indent = line.len() - line.trim_start().len();
        if let Some(help) = help_indent {
            if line.trim().is_empty() || indent > help {
                lines.push((path.to_owned(), index + 1, line.to_owned()));
                continue;
            }
            help_indent = None;
        }
        if line.trim() == "help" {
            help_indent = Some(indent);
        }
        if let Some(value) = line.trim().strip_prefix("source ") {
            let include = parse_quoted(value)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| Error::at(path, index + 1, "source requires a quoted path"))?;
            read_schema_lines(&root.join(include), root, active, lines)?;
        } else {
            lines.push((path.to_owned(), index + 1, line.to_owned()));
        }
    }
    active.pop();
    Ok(())
}

#[cfg(test)]
fn parse_schema_source(path: &Path, source: &str) -> Result<Schema, Error> {
    let lines = source
        .lines()
        .enumerate()
        .map(|(index, line)| (path.to_owned(), index + 1, line.to_owned()))
        .collect::<Vec<_>>();
    parse_schema_lines(path, &lines)
}

fn parse_schema_lines(root: &Path, lines: &[SourceLine]) -> Result<Schema, Error> {
    let mut title = String::from("Kernel Configuration");
    let mut symbols = Vec::new();
    let mut current: Option<Symbol> = None;
    let mut choices: Vec<Choice> = Vec::new();
    let mut active_choice = None;
    let mut help_indent = None;
    let mut menus = Vec::new();

    for (path, line_number, raw_line) in lines {
        let path = path.as_path();
        let line_number = *line_number;
        let line = raw_line.trim();
        if let Some(indent) = help_indent {
            let leading = raw_line.len() - raw_line.trim_start().len();
            if line.is_empty() || leading > indent {
                if let Some(symbol) = &mut current {
                    symbol.help.push_str(line);
                    symbol.help.push('\n');
                }
                continue;
            }
            help_indent = None;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(value) = line.strip_prefix("menu ") {
            finish_symbol(path, line_number, &mut current, &mut symbols)?;
            menus.push(
                parse_quoted(value)
                    .ok_or_else(|| Error::at(path, line_number, "menu requires a quoted title"))?,
            );
            continue;
        }
        if line == "endmenu" {
            finish_symbol(path, line_number, &mut current, &mut symbols)?;
            if menus.pop().is_none() {
                return Err(Error::at(path, line_number, "endmenu without menu"));
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("mainmenu ") {
            title = parse_quoted(value)
                .ok_or_else(|| Error::at(path, line_number, "mainmenu requires a quoted title"))?;
            continue;
        }
        if let Some(name) = line.strip_prefix("choice ") {
            if active_choice.is_some() {
                return Err(Error::at(
                    path,
                    line_number,
                    "nested choices are not supported",
                ));
            }
            finish_symbol(path, line_number, &mut current, &mut symbols)?;
            validate_name(name).map_err(|message| Error::at(path, line_number, message))?;
            active_choice = Some(choices.len());
            choices.push(Choice {
                name: name.to_owned(),
                prompt: name.to_owned(),
                default: String::new(),
                members: Vec::new(),
            });
            continue;
        }
        if line == "endchoice" {
            if active_choice.take().is_none() {
                return Err(Error::at(path, line_number, "endchoice without choice"));
            }
            finish_symbol(path, line_number, &mut current, &mut symbols)?;
            continue;
        }
        if let Some(name) = line.strip_prefix("config ") {
            finish_symbol(path, line_number, &mut current, &mut symbols)?;
            validate_name(name).map_err(|message| Error::at(path, line_number, message))?;
            if let Some(choice) = active_choice {
                choices[choice].members.push(name.to_owned());
            }
            current = Some(Symbol {
                name: name.to_owned(),
                kind: ValueKind::Bool,
                prompt: name.to_owned(),
                default: String::from("n"),
                dependencies: Vec::new(),
                range: None,
                help: String::new(),
                choice: active_choice,
                section: menus.join(" / "),
            });
            continue;
        }
        if current.is_none()
            && let Some(choice) = active_choice
        {
            if let Some(value) = line.strip_prefix("prompt ") {
                choices[choice].prompt = parse_quoted(value)
                    .ok_or_else(|| Error::at(path, line_number, "choice prompt must be quoted"))?;
            } else if let Some(value) = line.strip_prefix("default ") {
                validate_name(value).map_err(|message| Error::at(path, line_number, message))?;
                choices[choice].default = value.to_owned();
            } else {
                return Err(Error::at(path, line_number, "unsupported choice property"));
            }
            continue;
        }
        let symbol = current
            .as_mut()
            .ok_or_else(|| Error::at(path, line_number, "property outside a config block"))?;
        if let Some(rest) = line.strip_prefix("bool") {
            symbol.kind = ValueKind::Bool;
            symbol.prompt = parse_optional_prompt(rest, &symbol.name, path, line_number)?;
        } else if let Some(rest) = line.strip_prefix("int") {
            symbol.kind = ValueKind::Int;
            symbol.prompt = parse_optional_prompt(rest, &symbol.name, path, line_number)?;
        } else if let Some(rest) = line.strip_prefix("string") {
            symbol.kind = ValueKind::String;
            symbol.prompt = parse_optional_prompt(rest, &symbol.name, path, line_number)?;
        } else if let Some(value) = line.strip_prefix("default ") {
            if symbol.choice.is_some() {
                return Err(Error::at(
                    path,
                    line_number,
                    "set the default on the choice, not its members",
                ));
            }
            symbol.default = value.trim().to_owned();
        } else if let Some(expression) = line.strip_prefix("depends on ") {
            if symbol.choice.is_some() {
                return Err(Error::at(
                    path,
                    line_number,
                    "conditional choice members are not supported",
                ));
            }
            for expression in expression.split("&&") {
                let expression = expression.trim();
                let (inverted, dependency) = match expression.strip_prefix('!') {
                    Some(dependency) => (true, dependency.trim()),
                    None => (false, expression),
                };
                validate_name(dependency)
                    .map_err(|message| Error::at(path, line_number, message))?;
                symbol.dependencies.push(Dependency {
                    symbol: dependency.to_owned(),
                    inverted,
                });
            }
        } else if let Some(bounds) = line.strip_prefix("range ") {
            let mut values = bounds.split_whitespace();
            let minimum = parse_i64(values.next(), path, line_number)?;
            let maximum = parse_i64(values.next(), path, line_number)?;
            if values.next().is_some() || minimum > maximum {
                return Err(Error::at(path, line_number, "invalid integer range"));
            }
            symbol.range = Some((minimum, maximum));
        } else if line == "help" {
            help_indent = Some(raw_line.len() - raw_line.trim_start().len());
        } else {
            return Err(Error::at(
                path,
                line_number,
                format!("unsupported Kconfig statement: {line}"),
            ));
        }
    }
    if active_choice.is_some() {
        return Err(Error::new("unterminated choice"));
    }
    if !menus.is_empty() {
        return Err(Error::new("unterminated menu"));
    }
    finish_symbol(root, lines.len(), &mut current, &mut symbols)?;
    if symbols.is_empty() {
        return Err(Error::new("Kconfig does not declare any symbols"));
    }
    let mut names = BTreeSet::new();
    for name in symbols
        .iter()
        .map(|symbol| &symbol.name)
        .chain(choices.iter().map(|choice| &choice.name))
    {
        if !names.insert(name) {
            return Err(Error::new(format!("duplicate schema name {name}")));
        }
    }
    for choice in &choices {
        if choice.members.len() < 2 || !choice.members.contains(&choice.default) {
            return Err(Error::new(format!(
                "choice {} requires at least two members and a member default",
                choice.name
            )));
        }
    }
    for symbol in &symbols {
        if symbol.choice.is_some() && symbol.kind != ValueKind::Bool {
            return Err(Error::new(format!(
                "choice member {} must be boolean",
                symbol.name
            )));
        }
        for dependency in &symbol.dependencies {
            let Some(target) = symbols
                .iter()
                .find(|target| target.name == dependency.symbol)
            else {
                return Err(Error::new(format!(
                    "{} depends on unknown symbol {}",
                    symbol.name, dependency.symbol
                )));
            };
            if target.kind != ValueKind::Bool {
                return Err(Error::new(format!(
                    "{} depends on non-boolean symbol {}",
                    symbol.name, dependency.symbol
                )));
            }
        }
    }
    // Source order must not decide visibility. Check the dependency DAG once
    // before any recursive evaluation, including edges to disabled symbols.
    let mut active = BTreeSet::new();
    let mut complete = BTreeSet::new();
    for symbol in &symbols {
        check_dependency_cycles(symbol, &symbols, &mut active, &mut complete)?;
    }
    Ok(Schema {
        title,
        symbols,
        choices,
    })
}

fn check_dependency_cycles<'a>(
    symbol: &'a Symbol,
    symbols: &'a [Symbol],
    active: &mut BTreeSet<&'a str>,
    complete: &mut BTreeSet<&'a str>,
) -> Result<(), Error> {
    if complete.contains(symbol.name.as_str()) {
        return Ok(());
    }
    if !active.insert(&symbol.name) {
        return Err(Error::new(format!(
            "dependency cycle involving {}",
            symbol.name
        )));
    }
    for dependency in &symbol.dependencies {
        let parent = symbols
            .iter()
            .find(|item| item.name == dependency.symbol)
            .ok_or_else(|| Error::new("unknown dependency"))?;
        check_dependency_cycles(parent, symbols, active, complete)?;
    }
    active.remove(symbol.name.as_str());
    complete.insert(&symbol.name);
    Ok(())
}

fn finish_symbol(
    path: &Path,
    line: usize,
    current: &mut Option<Symbol>,
    symbols: &mut Vec<Symbol>,
) -> Result<(), Error> {
    if let Some(mut symbol) = current.take() {
        finalize_symbol(path, line, &mut symbol)?;
        symbols.push(symbol);
    }
    Ok(())
}

pub fn load_fragment(path: &Path) -> Result<BTreeMap<String, String>, Error> {
    let source = fs::read_to_string(path).map_err(|error| Error {
        path: Some(path.to_path_buf()),
        line: None,
        message: error.to_string(),
    })?;
    let mut values = BTreeMap::new();
    for (index, raw_line) in source.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || (line.starts_with('#') && !line.starts_with("# CONFIG_")) {
            continue;
        }
        if let Some(symbol) = line
            .strip_prefix("# CONFIG_")
            .and_then(|value| value.strip_suffix(" is not set"))
        {
            validate_name(symbol).map_err(|message| Error::at(path, line_number, message))?;
            if values
                .insert(symbol.to_owned(), String::from("n"))
                .is_some()
            {
                return Err(Error::at(
                    path,
                    line_number,
                    format!("duplicate assignment to CONFIG_{symbol}"),
                ));
            }
            continue;
        }
        let Some(assignment) = line.strip_prefix("CONFIG_") else {
            return Err(Error::at(path, line_number, "invalid configuration line"));
        };
        let Some((name, value)) = assignment.split_once('=') else {
            return Err(Error::at(
                path,
                line_number,
                "configuration value is missing",
            ));
        };
        validate_name(name).map_err(|message| Error::at(path, line_number, message))?;
        if values.insert(name.to_owned(), value.to_owned()).is_some() {
            return Err(Error::at(
                path,
                line_number,
                format!("duplicate assignment to CONFIG_{name}"),
            ));
        }
    }
    Ok(values)
}

pub fn resolve(
    schema: &Schema,
    overrides: BTreeMap<String, String>,
) -> Result<Configuration, Error> {
    resolve_values(schema, &overrides, true)
}

fn resolve_values(
    schema: &Schema,
    overrides: &BTreeMap<String, String>,
    strict: bool,
) -> Result<Configuration, Error> {
    let mut configuration = Configuration {
        values: schema
            .symbols
            .iter()
            .map(|symbol| (symbol.name.clone(), symbol.default.clone()))
            .collect(),
    };
    for (name, raw_value) in overrides {
        let symbol = schema
            .symbols
            .iter()
            .find(|symbol| symbol.name == *name)
            .ok_or_else(|| Error::new(format!("unknown configuration symbol CONFIG_{name}")))?;
        let value = normalize_literal(raw_value, &symbol.kind)
            .map_err(|message| Error::new(format!("CONFIG_{name}: {message}")))?;
        configuration.values.insert(name.clone(), value);
    }
    for choice in &schema.choices {
        let selected: Vec<_> = choice
            .members
            .iter()
            .filter(|name| overrides.get(*name).is_some_and(|value| value == "y"))
            .collect();
        let member = match selected.as_slice() {
            [member] => *member,
            [] => {
                let candidates = core::iter::once(&choice.default).chain(choice.members.iter());
                candidates
                    .into_iter()
                    .find(|name| !overrides.get(*name).is_some_and(|value| value == "n"))
                    .ok_or_else(|| {
                        Error::new(format!(
                            "choice {} requires one enabled member",
                            choice.name
                        ))
                    })?
            }
            _ => {
                return Err(Error::new(format!(
                    "choice {} has multiple enabled members: {}",
                    choice.name,
                    selected
                        .iter()
                        .map(|name| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        };
        for name in &choice.members {
            configuration.values.insert(
                name.clone(),
                if name == member { "y" } else { "n" }.to_owned(),
            );
        }
    }
    let inactive: Vec<_> = schema
        .symbols
        .iter()
        .filter(|symbol| !dependency_is_active(schema, symbol, &configuration))
        .collect();
    for symbol in inactive {
        if strict
            && overrides.contains_key(&symbol.name)
            && !(symbol.kind == ValueKind::Bool && configuration.value(&symbol.name) == Some("n"))
        {
            return Err(Error::new(format!(
                "CONFIG_{} is not available; requires {}",
                symbol.name,
                symbol
                    .dependencies
                    .iter()
                    .map(|dependency| format!(
                        "{}CONFIG_{}",
                        if dependency.inverted { "!" } else { "" },
                        dependency.symbol
                    ))
                    .collect::<Vec<_>>()
                    .join(" && ")
            )));
        }
        configuration.values.remove(&symbol.name);
    }
    validate_configuration(schema, &configuration)?;
    Ok(configuration)
}

fn dependency_is_active(schema: &Schema, symbol: &Symbol, configuration: &Configuration) -> bool {
    symbol.dependencies.iter().all(|dependency| {
        let enabled = configuration.value(&dependency.symbol) == Some("y")
            && schema
                .symbols
                .iter()
                .find(|item| item.name == dependency.symbol)
                .is_some_and(|parent| dependency_is_active(schema, parent, configuration));
        enabled != dependency.inverted
    })
}

pub fn load_and_validate(kconfig: &Path, config: &Path) -> Result<(Schema, Configuration), Error> {
    let schema = parse_schema(kconfig)?;
    let configuration = resolve(&schema, load_fragment(config)?)?;
    Ok((schema, configuration))
}

/// Migrates an existing configuration while preserving its applicable values.
/// Removed names are returned for the CLI to report. Builds and defconfig still
/// use strict `resolve`, so misspelled or stale keys cannot be ignored silently.
pub fn resolve_existing(
    schema: &Schema,
    mut overrides: BTreeMap<String, String>,
) -> Result<(Configuration, Vec<String>), Error> {
    let mut removed = Vec::new();
    overrides.retain(|name, _| {
        let known = schema.symbols.iter().any(|symbol| symbol.name == *name);
        if !known {
            removed.push(name.clone());
        }
        known
    });
    let configuration = resolve_values(schema, &overrides, false)?;
    removed.extend(
        overrides
            .keys()
            .filter(|name| configuration.value(name).is_none())
            .cloned(),
    );
    Ok((configuration, removed))
}

pub fn write_config(
    path: &Path,
    schema: &Schema,
    configuration: &Configuration,
) -> Result<(), Error> {
    let mut output = format!(
        "#\n# Automatically generated file; DO NOT EDIT.\n# {}\n#\n",
        schema.title
    );
    let mut section = "";
    for symbol in &schema.symbols {
        if !dependency_is_met(symbol, configuration)
            || (symbol.choice.is_some() && configuration.value(&symbol.name) != Some("y"))
        {
            continue;
        }
        if symbol.section != section {
            section = &symbol.section;
            output.push_str(&format!("\n# {section}\n"));
        }
        let value = configuration
            .value(&symbol.name)
            .ok_or_else(|| Error::new(format!("CONFIG_{} has no value", symbol.name)))?;
        match (&symbol.kind, value) {
            (ValueKind::Bool, "n") => {
                output.push_str(&format!("# CONFIG_{} is not set\n", symbol.name));
            }
            (ValueKind::String, value) => {
                output.push_str(&format!("CONFIG_{}={}\n", symbol.name, quote(value)));
            }
            _ => output.push_str(&format!("CONFIG_{}={value}\n", symbol.name)),
        }
    }
    fs::write(path, output).map_err(|error| Error {
        path: Some(path.to_path_buf()),
        line: None,
        message: error.to_string(),
    })
}

pub fn dependency_is_met(symbol: &Symbol, configuration: &Configuration) -> bool {
    symbol.dependencies.iter().all(|dependency| {
        (configuration.value(&dependency.symbol) == Some("y")) != dependency.inverted
    })
}

fn validate_configuration(schema: &Schema, configuration: &Configuration) -> Result<(), Error> {
    for symbol in &schema.symbols {
        if !dependency_is_met(symbol, configuration) {
            continue;
        }
        let value = configuration
            .value(&symbol.name)
            .ok_or_else(|| Error::new(format!("CONFIG_{} has no value", symbol.name)))?;
        if let Some((minimum, maximum)) = symbol.range {
            let value = value
                .parse::<i64>()
                .map_err(|_| Error::new(format!("CONFIG_{} is not an integer", symbol.name)))?;
            if !(minimum..=maximum).contains(&value) {
                return Err(Error::new(format!(
                    "CONFIG_{} must be in the range {minimum}..={maximum}",
                    symbol.name
                )));
            }
        }
    }
    Ok(())
}

fn finalize_symbol(path: &Path, line: usize, symbol: &mut Symbol) -> Result<(), Error> {
    symbol.default = normalize_literal(&symbol.default, &symbol.kind)
        .map_err(|message| Error::at(path, line, format!("{}: {message}", symbol.name)))?;
    if symbol.kind != ValueKind::Int && symbol.range.is_some() {
        return Err(Error::at(path, line, "range is valid only for int symbols"));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        Err("symbol names must contain only A-Z, 0-9, and underscore")
    } else {
        Ok(())
    }
}

fn normalize_literal(value: &str, kind: &ValueKind) -> Result<String, &'static str> {
    match kind {
        ValueKind::Bool if matches!(value, "y" | "n") => Ok(value.to_owned()),
        ValueKind::Bool => Err("boolean value must be y or n"),
        ValueKind::Int => value
            .parse::<i64>()
            .map(|parsed| parsed.to_string())
            .map_err(|_| "integer value is invalid"),
        ValueKind::String => parse_quoted(value).ok_or("string value must be quoted"),
    }
}

fn parse_optional_prompt(
    value: &str,
    fallback: &str,
    path: &Path,
    line: usize,
) -> Result<String, Error> {
    let value = value.trim();
    if value.is_empty() {
        Ok(fallback.to_owned())
    } else {
        parse_quoted(value).ok_or_else(|| Error::at(path, line, "prompt must be quoted"))
    }
}

pub fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn parse_quoted(value: &str) -> Option<String> {
    let value = value.trim().strip_prefix('"')?.strip_suffix('"')?;
    let mut result = String::new();
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.next()? {
                escaped @ ('\\' | '"') => result.push(escaped),
                _ => return None,
            },
            '"' => return None,
            character if character.is_control() => return None,
            character => result.push(character),
        }
    }
    Some(result)
}

fn parse_i64(value: Option<&str>, path: &Path, line: usize) -> Result<i64, Error> {
    value
        .ok_or_else(|| Error::at(path, line, "integer range bound is missing"))?
        .parse()
        .map_err(|_| Error::at(path, line, "integer range bound is invalid"))
}

pub fn read_line() -> io::Result<String> {
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "configuration input ended before selection completed",
        ));
    }
    Ok(input.trim().to_owned())
}

#[cfg(test)]
mod tests;
