// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Borrowed decoders for standard FDT property encodings.
//!
//! Values remain borrowed from the validated blob and cannot outlive their
//! visitor callback. This module knows byte encodings, not binding-specific
//! property names or device policy.

use core::str;

/// A borrowed property from the structure block of a validated FDT blob.
#[derive(Clone, Copy)]
pub struct Property<'a> {
    pub(super) name: &'a str,
    pub(super) value: &'a [u8],
}

impl<'a> Property<'a> {
    pub const fn name(self) -> &'a str {
        self.name
    }

    pub const fn bytes(self) -> &'a [u8] {
        self.value
    }

    pub fn u32(self) -> Result<u32, PropertyError> {
        decode_u32(self.value)
    }

    pub fn u64(self) -> Result<u64, PropertyError> {
        let bytes: [u8; 8] = self
            .value
            .try_into()
            .map_err(|_| PropertyError::InvalidLength)?;
        Ok(u64::from_be_bytes(bytes))
    }

    /// Decodes a one- or two-cell integer property.
    pub fn integer(self) -> Result<u64, PropertyError> {
        match self.value.len() {
            4 => self.u32().map(u64::from),
            8 => self.u64(),
            _ => Err(PropertyError::InvalidLength),
        }
    }

    pub fn string(self) -> Result<&'a str, PropertyError> {
        let value = self
            .value
            .strip_suffix(&[0])
            .ok_or(PropertyError::MissingTerminator)?;
        if value.contains(&0) {
            return Err(PropertyError::EmbeddedTerminator);
        }
        str::from_utf8(value).map_err(|_| PropertyError::InvalidUtf8)
    }

    pub fn strings(self) -> Result<StringList<'a>, PropertyError> {
        let value = self
            .value
            .strip_suffix(&[0])
            .ok_or(PropertyError::MissingTerminator)?;
        let value = str::from_utf8(value).map_err(|_| PropertyError::InvalidUtf8)?;
        Ok(StringList {
            items: value.split('\0'),
        })
    }

    pub fn contains_string(self, expected: &str) -> Result<bool, PropertyError> {
        Ok(self.strings()?.any(|item| item == expected))
    }

    pub fn cells(self) -> Result<CellList<'a>, PropertyError> {
        if !self.value.len().is_multiple_of(4) {
            return Err(PropertyError::InvalidLength);
        }
        Ok(CellList {
            remaining: self.value,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PropertyError {
    EmbeddedTerminator,
    InvalidLength,
    InvalidUtf8,
    MissingTerminator,
}

pub struct StringList<'a> {
    items: str::Split<'a, char>,
}

impl<'a> Iterator for StringList<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        self.items.next()
    }
}

pub struct CellList<'a> {
    remaining: &'a [u8],
}

impl Iterator for CellList<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        let bytes: [u8; 4] = self.remaining.get(..4)?.try_into().ok()?;
        self.remaining = &self.remaining[4..];
        Some(u32::from_be_bytes(bytes))
    }
}

pub(super) fn decode_u32(value: &[u8]) -> Result<u32, PropertyError> {
    let bytes: [u8; 4] = value.try_into().map_err(|_| PropertyError::InvalidLength)?;
    Ok(u32::from_be_bytes(bytes))
}
