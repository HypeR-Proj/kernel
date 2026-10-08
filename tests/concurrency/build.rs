// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

fn main() {
    // Only this test crate compiles the included mechanisms with Loom atomics.
    // Do not set global RUSTFLAGS: hyper-core and the kernel stay uninstrumented.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(loom)");
    println!("cargo:rustc-cfg=loom");
}
