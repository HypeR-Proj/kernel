// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::error::Error;
use std::path::PathBuf;

#[path = "../build_support/config.rs"]
mod config;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    let kernel_root = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join("..");
    config::export(&kernel_root)
}
