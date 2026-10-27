// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Validated filesystem component names.

/// Maximum encoded bytes in one filesystem component.
pub const MAX_NAME_BYTES: usize = 255;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NameError {
    Empty,
    TooLong,
    Reserved,
    ContainsNul,
    ContainsSeparator,
}

/// One borrowed, non-special filesystem component.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Name<'name>(&'name str);

impl<'name> Name<'name> {
    pub fn new(value: &'name str) -> Result<Self, NameError> {
        if value.is_empty() {
            return Err(NameError::Empty);
        }
        if value.len() > MAX_NAME_BYTES {
            return Err(NameError::TooLong);
        }
        if value == "." || value == ".." {
            return Err(NameError::Reserved);
        }
        if value.as_bytes().contains(&0) {
            return Err(NameError::ContainsNul);
        }
        if value.as_bytes().contains(&b'/') {
            return Err(NameError::ContainsSeparator);
        }
        Ok(Self(value))
    }

    pub(super) const fn from_validated(value: &'name str) -> Self {
        Self(value)
    }

    pub const fn as_str(self) -> &'name str {
        self.0
    }

    pub const fn as_bytes(self) -> &'name [u8] {
        self.0.as_bytes()
    }
}
