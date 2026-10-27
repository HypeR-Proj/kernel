// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded filesystem paths with allocation-free component iteration.

use super::{MAX_NAME_BYTES, Name};

/// Maximum bytes accepted from one path-bearing interface.
pub const MAX_PATH_BYTES: usize = 4096;
/// Independent bound on resolver work even when every component is short.
pub const MAX_PATH_COMPONENTS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathError {
    Empty,
    TooLong,
    ContainsNul,
    EmptyComponent,
    ComponentTooLong,
    TooManyComponents,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathComponent<'path> {
    Current,
    Parent,
    Name(Name<'path>),
}

/// A syntactically validated path.
///
/// `.` and `..` remain typed components because containment belongs to the VFS
/// resolver and its traversal floor. Empty components are rejected so callers
/// never disagree about repeated or trailing separators.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Path<'path> {
    value: &'path str,
    components: &'path str,
    absolute: bool,
    component_count: usize,
}

impl<'path> Path<'path> {
    pub fn new(value: &'path str) -> Result<Self, PathError> {
        if value.is_empty() {
            return Err(PathError::Empty);
        }
        if value.len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        if value.as_bytes().contains(&0) {
            return Err(PathError::ContainsNul);
        }

        let absolute = value.starts_with('/');
        let components = if absolute {
            value.strip_prefix('/').ok_or(PathError::EmptyComponent)?
        } else {
            value
        };
        if components.is_empty() {
            return Ok(Self {
                value,
                components,
                absolute,
                component_count: 0,
            });
        }

        let mut component_count = 0usize;
        for component in components.split('/') {
            if component.is_empty() {
                return Err(PathError::EmptyComponent);
            }
            if component.len() > MAX_NAME_BYTES {
                return Err(PathError::ComponentTooLong);
            }
            component_count = component_count
                .checked_add(1)
                .ok_or(PathError::TooManyComponents)?;
            if component_count > MAX_PATH_COMPONENTS {
                return Err(PathError::TooManyComponents);
            }
        }
        Ok(Self {
            value,
            components,
            absolute,
            component_count,
        })
    }

    pub const fn is_absolute(self) -> bool {
        self.absolute
    }

    pub const fn component_count(self) -> usize {
        self.component_count
    }

    pub const fn as_str(self) -> &'path str {
        self.value
    }

    pub fn components(self) -> Components<'path> {
        Components {
            inner: self.components.split('/'),
            remaining: self.component_count,
        }
    }
}

pub struct Components<'path> {
    inner: core::str::Split<'path, char>,
    remaining: usize,
}

impl<'path> Iterator for Components<'path> {
    type Item = PathComponent<'path>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let component = self.inner.next()?;
        self.remaining -= 1;
        match component {
            "." => Some(PathComponent::Current),
            ".." => Some(PathComponent::Parent),
            name => Some(PathComponent::Name(Name::from_validated(name))),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for Components<'_> {}
