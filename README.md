<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# HypeR Kernel

This directory is the independently buildable HypeR type-1 hypervisor kernel.
It owns privileged runtime policy, architecture and HAL implementations,
physical drivers, kernel configuration, image construction, and kernel-only
verification. Repository-level SDK and application composition remain outside
this directory.

## Layout

| Path | Responsibility |
| --- | --- |
| `src/` | Kernel, architecture, HAL, driver, VM, and reusable mechanism sources |
| `configs/` and `Kconfig` | Supported platform configurations and configuration schema |
| `docs/` | Kernel architecture, ABI, boot, VM, and platform contracts |
| `tests/` | Host, self-test, image, and QEMU kernel verification |
| `tools/` | Kconfig, kallsyms, and pinned Linux guest-payload tooling |
| `.cargo/`, `Cargo.toml`, and `build.rs` | Rust target, build, assembly, and linker integration |

## Build independently

From this directory:

```sh
make defconfig
make image ARCH=aarch64
make check ARCH=aarch64
```

The kernel consumes the dependency-free Native ABI crate from `../sdk/abi`.
It does not build the Native runtime or applications. Use the repository
root Makefile for complete SDK, initramfs, and production boot composition.

Generated artifacts and configuration remain under this component's `target/`
and `.config` paths whether commands begin here or at the repository root.
