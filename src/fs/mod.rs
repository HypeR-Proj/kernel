// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Reusable filesystem mechanisms consumed by kernel storage policy.

mod name;
mod node;
mod path;

pub mod file_data;
pub mod ramfs;

pub use name::{MAX_NAME_BYTES, Name, NameError};
pub use node::{NodeAttributes, NodeId, NodeKind};
pub use path::{MAX_PATH_BYTES, MAX_PATH_COMPONENTS, Path, PathComponent, PathError};
