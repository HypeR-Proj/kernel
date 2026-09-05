// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
};

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
    pub dependency: Option<Dependency>,
    pub range: Option<(i64, i64)>,
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

pub fn parse_schema(path: &Path) -> Result<Schema, Error> {
    let source = fs::read_to_string(path).map_err(|error| Error {
        path: Some(path.to_path_buf()),
        line: None,
        message: error.to_string(),
    })?;
    let mut title = String::from("Kernel Configuration");
    let mut symbols = Vec::new();
    let mut current: Option<Symbol> = None;

    for (index, raw_line) in source.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with("menu ")
            || line == "endmenu"
        {
            continue;
        }
        if let Some(value) = line.strip_prefix("mainmenu ") {
            title = parse_quoted(value)
                .ok_or_else(|| Error::at(path, line_number, "mainmenu requires a quoted title"))?;
            continue;
        }
        if let Some(name) = line.strip_prefix("config ") {
            if let Some(mut symbol) = current.take() {
                finalize_symbol(path, line_number, &mut symbol)?;
                symbols.push(symbol);
            }
            validate_name(name).map_err(|message| Error::at(path, line_number, message))?;
            current = Some(Symbol {
                name: name.to_owned(),
                kind: ValueKind::Bool,
                prompt: name.to_owned(),
                default: String::from("n"),
                dependency: None,
                range: None,
            });
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
            symbol.default = value.trim().to_owned();
        } else if let Some(expression) = line.strip_prefix("depends on ") {
            let expression = expression.trim();
            let (inverted, dependency) = match expression.strip_prefix('!') {
                Some(dependency) => (true, dependency.trim()),
                None => (false, expression),
            };
            validate_name(dependency).map_err(|message| Error::at(path, line_number, message))?;
            symbol.dependency = Some(Dependency {
                symbol: dependency.to_owned(),
                inverted,
            });
        } else if let Some(bounds) = line.strip_prefix("range ") {
            let mut values = bounds.split_whitespace();
            let minimum = parse_i64(values.next(), path, line_number)?;
            let maximum = parse_i64(values.next(), path, line_number)?;
            if values.next().is_some() || minimum > maximum {
                return Err(Error::at(path, line_number, "invalid integer range"));
            }
            symbol.range = Some((minimum, maximum));
        } else {
            return Err(Error::at(
                path,
                line_number,
                format!("unsupported Kconfig statement: {line}"),
            ));
        }
    }
    if let Some(mut symbol) = current {
        finalize_symbol(path, source.lines().count(), &mut symbol)?;
        symbols.push(symbol);
    }
    if symbols.is_empty() {
        return Err(Error::new("Kconfig does not declare any symbols"));
    }
    for symbol in &symbols {
        if let Some(dependency) = &symbol.dependency {
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
    Ok(Schema { title, symbols })
}

pub fn defaults(schema: &Schema) -> Configuration {
    Configuration {
        values: schema
            .symbols
            .iter()
            .map(|symbol| (symbol.name.clone(), symbol.default.clone()))
            .collect(),
    }
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
            values.insert(symbol.to_owned(), String::from("n"));
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
        values.insert(name.to_owned(), value.to_owned());
    }
    Ok(values)
}

pub fn resolve(
    schema: &Schema,
    overrides: BTreeMap<String, String>,
) -> Result<Configuration, Error> {
    let mut configuration = defaults(schema);
    for (name, raw_value) in overrides {
        let symbol = schema
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .ok_or_else(|| Error::new(format!("unknown configuration symbol CONFIG_{name}")))?;
        let value = normalize_literal(&raw_value, &symbol.kind)
            .map_err(|message| Error::new(format!("CONFIG_{name}: {message}")))?;
        configuration.values.insert(name, value);
    }
    validate_configuration(schema, &mut configuration)?;
    Ok(configuration)
}

pub fn load_and_validate(kconfig: &Path, config: &Path) -> Result<(Schema, Configuration), Error> {
    let schema = parse_schema(kconfig)?;
    let configuration = resolve(&schema, load_fragment(config)?)?;
    Ok((schema, configuration))
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
    for symbol in &schema.symbols {
        let value = configuration
            .value(&symbol.name)
            .ok_or_else(|| Error::new(format!("CONFIG_{} has no value", symbol.name)))?;
        match (&symbol.kind, value) {
            (ValueKind::Bool, "n") => {
                output.push_str(&format!("# CONFIG_{} is not set\n", symbol.name));
            }
            (ValueKind::String, value) => {
                output.push_str(&format!("CONFIG_{}=\"{value}\"\n", symbol.name));
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
    let Some(dependency) = &symbol.dependency else {
        return true;
    };
    let enabled = configuration.value(&dependency.symbol) == Some("y");
    enabled != dependency.inverted
}

fn validate_configuration(schema: &Schema, configuration: &mut Configuration) -> Result<(), Error> {
    for symbol in &schema.symbols {
        if !dependency_is_met(symbol, configuration) {
            if symbol.kind == ValueKind::Bool {
                configuration
                    .values
                    .insert(symbol.name.clone(), String::from("n"));
            }
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

fn parse_quoted(value: &str) -> Option<String> {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .map(str::to_owned)
}

fn parse_i64(value: Option<&str>, path: &Path, line: usize) -> Result<i64, Error> {
    value
        .ok_or_else(|| Error::at(path, line, "integer range bound is missing"))?
        .parse()
        .map_err(|_| Error::at(path, line, "integer range bound is invalid"))
}

pub fn read_line() -> io::Result<String> {
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn require_ok<T, E: fmt::Debug>(result: Result<T, E>) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("required success, received {error:?}"),
        }
    }

    fn test_schema() -> Schema {
        Schema {
            title: String::from("Test"),
            symbols: vec![
                Symbol {
                    name: String::from("PARENT"),
                    kind: ValueKind::Bool,
                    prompt: String::from("Parent"),
                    default: String::from("n"),
                    dependency: None,
                    range: None,
                },
                Symbol {
                    name: String::from("CHILD"),
                    kind: ValueKind::Bool,
                    prompt: String::from("Child"),
                    default: String::from("y"),
                    dependency: Some(Dependency {
                        symbol: String::from("PARENT"),
                        inverted: false,
                    }),
                    range: None,
                },
                Symbol {
                    name: String::from("COUNT"),
                    kind: ValueKind::Int,
                    prompt: String::from("Count"),
                    default: String::from("4"),
                    dependency: None,
                    range: Some((1, 8)),
                },
            ],
        }
    }

    #[test]
    fn resolves_dependencies_and_integer_ranges() {
        let configuration = require_ok(resolve(&test_schema(), BTreeMap::new()));
        assert_eq!(configuration.value("CHILD"), Some("n"));

        let mut invalid = BTreeMap::new();
        invalid.insert(String::from("COUNT"), String::from("9"));
        assert!(resolve(&test_schema(), invalid).is_err());
    }

    #[test]
    fn parses_the_supported_kconfig_subset() {
        let path =
            std::env::temp_dir().join(format!("hyper-kconfig-test-{}-Kconfig", std::process::id()));
        require_ok(fs::write(
            &path,
            "mainmenu \"Test\"\nconfig ENABLED\n    bool \"Enabled\"\n    default y\n",
        ));
        let schema = require_ok(parse_schema(&path));
        let remove_result = fs::remove_file(&path);
        require_ok(remove_result);

        assert_eq!(schema.title, "Test");
        assert_eq!(schema.symbols.len(), 1);
        assert_eq!(schema.symbols[0].default, "y");
    }
}
