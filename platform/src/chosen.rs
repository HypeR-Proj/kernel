// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free discovery of Linux-style DTB `/chosen` properties.

use core::str;

use super::{
    PhysicalRange,
    fdt::{NodeId, NodeResources, NodeVisitor, Property},
};

pub const MAX_COMMAND_LINE_SIZE: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    DuplicateBootargs,
    DuplicateKaslrSeed,
    DuplicateRngSeed,
    DuplicateInitrdEnd,
    DuplicateInitrdStart,
    InvalidEncoding,
    InvalidInitrd,
    InvalidKaslrSeed,
    InvalidRngSeed,
    InvalidTree,
    TooLong,
}

/// A validated copy of the DTB `/chosen/bootargs` string.
#[derive(Clone, Copy)]
pub struct CommandLine {
    bytes: [u8; MAX_COMMAND_LINE_SIZE],
    length: usize,
}

impl CommandLine {
    const EMPTY: Self = Self {
        bytes: [0; MAX_COMMAND_LINE_SIZE],
        length: 0,
    };

    fn from_property(value: &[u8]) -> Result<Self, Error> {
        let bytes = value.strip_suffix(&[0]).ok_or(Error::InvalidEncoding)?;
        if bytes.contains(&0) {
            return Err(Error::InvalidEncoding);
        }
        let text = str::from_utf8(bytes).map_err(|_| Error::InvalidEncoding)?;
        if text.len() > MAX_COMMAND_LINE_SIZE {
            return Err(Error::TooLong);
        }
        let mut command_line = Self::EMPTY;
        command_line.bytes[..text.len()].copy_from_slice(text.as_bytes());
        command_line.length = text.len();
        Ok(command_line)
    }

    pub fn parse(value: &str) -> Result<Self, Error> {
        if value.as_bytes().contains(&0) {
            return Err(Error::InvalidEncoding);
        }
        if value.len() > MAX_COMMAND_LINE_SIZE {
            return Err(Error::TooLong);
        }
        let mut command_line = Self::EMPTY;
        command_line.bytes[..value.len()].copy_from_slice(value.as_bytes());
        command_line.length = value.len();
        Ok(command_line)
    }

    pub fn as_str(&self) -> &str {
        // Construction validates UTF-8 and only copies those bytes. Returning
        // an empty string on an internal invariant violation keeps this
        // accessor memory-safe even if a future constructor regresses.
        str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }

    /// Returns the value of the first `name=value` argument.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.as_str().split_ascii_whitespace().find_map(|argument| {
            let (argument_name, value) = argument.split_once('=')?;
            (argument_name == name).then_some(value)
        })
    }

    /// Reports whether a flag or valued argument with this name is present.
    pub fn contains(&self, name: &str) -> bool {
        self.as_str().split_ascii_whitespace().any(|argument| {
            argument == name
                || argument
                    .split_once('=')
                    .is_some_and(|(argument_name, _)| argument_name == name)
        })
    }

    /// Removes named flags and `name=value` arguments without changing the
    /// order or bytes of other arguments. Separating whitespace is normalized.
    pub fn without_arguments(&self, names: &[&str]) -> Self {
        let mut remaining = Self::EMPTY;
        for argument in self.as_str().split_ascii_whitespace() {
            let name = argument.split_once('=').map_or(argument, |(name, _)| name);
            if names.contains(&name) {
                continue;
            }
            if remaining.length != 0 {
                remaining.bytes[remaining.length] = b' ';
                remaining.length += 1;
            }
            let end = remaining.length + argument.len();
            // Removing arguments and collapsing separators cannot grow the
            // validated source, whose length already fits this buffer.
            remaining.bytes[remaining.length..end].copy_from_slice(argument.as_bytes());
            remaining.length = end;
        }
        remaining
    }
}

#[derive(Clone, Copy)]
pub struct Properties {
    command_line: Option<CommandLine>,
    command_line_error: Option<Error>,
    kaslr_seed: Option<u64>,
    kaslr_seed_error: Option<Error>,
    rng_seed: Option<u64>,
    rng_seed_error: Option<Error>,
    initial_ramdisk: Option<PhysicalRange>,
}

impl Properties {
    pub const fn command_line(&self) -> Option<&CommandLine> {
        self.command_line.as_ref()
    }

    pub const fn kaslr_seed(&self) -> Option<u64> {
        self.kaslr_seed
    }

    /// A bounded fold of the bootloader's rng-seed bytes for the general PRNG.
    /// This is diversification, not an entropy estimate or cryptographic hash.
    pub const fn rng_seed(&self) -> Option<u64> {
        self.rng_seed
    }

    pub const fn rng_seed_error(&self) -> Option<Error> {
        self.rng_seed_error
    }

    pub const fn command_line_error(&self) -> Option<Error> {
        self.command_line_error
    }

    pub const fn kaslr_seed_error(&self) -> Option<Error> {
        self.kaslr_seed_error
    }

    pub const fn initial_ramdisk(&self) -> Option<PhysicalRange> {
        self.initial_ramdisk
    }
}

/// Extracts boot configuration during the allocation-free initial FDT walk.
pub struct Discovery {
    depth: usize,
    chosen_depth: Option<usize>,
    command_line: Option<CommandLine>,
    command_line_error: Option<Error>,
    kaslr_seed: Option<u64>,
    kaslr_seed_error: Option<Error>,
    rng_seed: Option<u64>,
    rng_seed_error: Option<Error>,
    initrd_start: Option<u64>,
    initrd_end: Option<u64>,
}

impl Discovery {
    pub const fn new() -> Self {
        Self {
            depth: 0,
            chosen_depth: None,
            command_line: None,
            command_line_error: None,
            kaslr_seed: None,
            kaslr_seed_error: None,
            rng_seed: None,
            rng_seed_error: None,
            initrd_start: None,
            initrd_end: None,
        }
    }

    pub fn finish(self) -> Result<Properties, Error> {
        if self.depth != 0 {
            return Err(Error::InvalidTree);
        }
        let initial_ramdisk = match (self.initrd_start, self.initrd_end) {
            (None, None) => None,
            (Some(start), Some(end)) => Some(
                PhysicalRange::new(start, end.checked_sub(start).ok_or(Error::InvalidInitrd)?)
                    .ok_or(Error::InvalidInitrd)?,
            ),
            _ => return Err(Error::InvalidInitrd),
        };
        Ok(Properties {
            command_line: self.command_line,
            command_line_error: self.command_line_error,
            kaslr_seed: self.kaslr_seed,
            kaslr_seed_error: self.kaslr_seed_error,
            rng_seed: self.rng_seed,
            rng_seed_error: self.rng_seed_error,
            initial_ramdisk,
        })
    }

    fn record_property(&mut self, property: Property<'_>) -> Result<(), Error> {
        match property.name() {
            "bootargs" => {
                if self.command_line.is_some() || self.command_line_error.is_some() {
                    self.command_line = None;
                    self.command_line_error = Some(Error::DuplicateBootargs);
                } else {
                    match CommandLine::from_property(property.bytes()) {
                        Ok(command_line) => self.command_line = Some(command_line),
                        Err(error) => self.command_line_error = Some(error),
                    }
                }
            }
            "kaslr-seed" => {
                if self.kaslr_seed.is_some() || self.kaslr_seed_error.is_some() {
                    self.kaslr_seed = None;
                    self.kaslr_seed_error = Some(Error::DuplicateKaslrSeed);
                } else {
                    match property.u64() {
                        Ok(seed) => self.kaslr_seed = Some(seed),
                        Err(_) => self.kaslr_seed_error = Some(Error::InvalidKaslrSeed),
                    }
                }
            }
            "rng-seed" => {
                if self.rng_seed.is_some() || self.rng_seed_error.is_some() {
                    self.rng_seed = None;
                    self.rng_seed_error = Some(Error::DuplicateRngSeed);
                } else if property.bytes().is_empty() || property.bytes().len() > 256 {
                    self.rng_seed_error = Some(Error::InvalidRngSeed);
                } else {
                    let mut random = hyper::random::Random::new(property.bytes().len() as u64);
                    for chunk in property.bytes().chunks(8) {
                        let mut word = [0; 8];
                        word[..chunk.len()].copy_from_slice(chunk);
                        random.mix(u64::from_le_bytes(word));
                    }
                    self.rng_seed = Some(random.next_u64());
                }
            }
            "linux,initrd-start" => {
                if self.initrd_start.is_some() {
                    return Err(Error::DuplicateInitrdStart);
                }
                self.initrd_start = Some(property.integer().map_err(|_| Error::InvalidInitrd)?);
            }
            "linux,initrd-end" => {
                if self.initrd_end.is_some() {
                    return Err(Error::DuplicateInitrdEnd);
                }
                self.initrd_end = Some(property.integer().map_err(|_| Error::InvalidInitrd)?);
            }
            _ => {}
        }
        Ok(())
    }
}

impl Default for Discovery {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeVisitor for Discovery {
    type Error = Error;

    fn begin_node(&mut self, _id: NodeId, name: &str) -> Result<(), Self::Error> {
        let depth = self.depth.checked_add(1).ok_or(Error::InvalidTree)?;
        self.depth = depth;
        if depth == 2 && name == "chosen" {
            self.chosen_depth = Some(depth);
        }
        Ok(())
    }

    fn property(&mut self, _id: NodeId, property: Property<'_>) -> Result<(), Self::Error> {
        if self.chosen_depth != Some(self.depth) {
            return Ok(());
        }
        self.record_property(property)
    }

    fn end_node(&mut self, _node: NodeResources<'_>) -> Result<(), Self::Error> {
        if self.depth == 0 {
            return Err(Error::InvalidTree);
        }
        if self.chosen_depth == Some(self.depth) {
            self.chosen_depth = None;
        }
        self.depth -= 1;
        Ok(())
    }
}
