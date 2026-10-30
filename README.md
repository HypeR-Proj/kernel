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
| `src/` | Kernel binary: boot, entry, runtime ownership and subsystem policy |
| `src/diagnostics/` | Crash handling, symbols, inspection, logging, profiling and trace control |
| `src/fs/` | Filesystem primitives, archive import, VFS ownership and file cache |
| `src/mm/` | Runtime memory ownership, buddy/heap allocators, address spaces and reclamation |
| `src/mm/allocator/` | Buddy and heap mechanisms plus global allocator policy |
| `src/vm/serial/` and `src/vm/timer/` | VM serial transport and guest-timer binding/recovery |
| `core/src/` | `hyper-core`: shared types, collections, synchronization, HAL contracts and foundational memory mechanisms |
| `platform/src/` | `hyper-platform`: firmware parsing, resource descriptions and board-specific discovery |
| `drivers/src/` | `hyper-drivers`: physical devices and firmware-interface drivers |
| `vm-models/src/` | `hyper-vm-models`: guest-visible device models and portable virtualization contracts |
| `hal/src/` | `hyper-hal`: selected machine interface and private architecture implementations |
| `hal/src/arch/<arch>/` | Architecture mechanisms grouped into memory, IRQ, Native user execution and VM subsystems where implemented |
| `trace/src/` | `hyper-trace`: event definitions, macros and bounded recording |
| `configs/` and `Kconfig` | Supported platform configurations and configuration schema |
| `docs/` | Kernel architecture, ABI, boot, VM, and platform contracts |
| `tests/` | Host, self-test, image, and QEMU kernel verification |
| `tools/` | Kconfig, kallsyms, and pinned Linux guest-payload tooling |
| `.cargo/`, `Cargo.toml`, and `build.rs` | Rust target, build, assembly, and linker integration |

## Build independently

From this directory:

```sh
make defconfig
make menuconfig
make image ARCH=aarch64
make check ARCH=aarch64
```

The kernel consumes the dependency-free Native ABI crate from `../sdk/abi`.
It does not build the Native runtime or applications. Use the repository
root Makefile for complete SDK, initramfs, and production boot composition.
Its `make menuconfig` opens the product menu, with an entry into this kernel
menu alongside separate stage1 and formal-system selections.

Generated artifacts and configuration remain under this component's `target/`
and `.config` paths whether commands begin here or at the repository root.

Both Make entry points share target triples and QEMU defaults from
`mk/platform.mk`. Kernel feature defaults and saved selections are resolved
from Kconfig, including architecture-specific availability.

Kernel functionality, architecture and numeric limits are selected in
`make menuconfig`. `KERNEL_FEATURES` is an explicit override for isolated builds.
Test instrumentation requires the independent,
default-off `kernel-test` feature together with a specific test selector. See
the [configuration contract](docs/usage/configuration.md) for exact defaults, disabled
behavior, and test-image safeguards.

`make test-smmuv3 ARCH=aarch64` builds the dedicated PCI DMA isolation fixture
and runs it against QEMU's standard SMMUv3 model. See the
[driver and acceptance contract](docs/design/smmuv3.md) for coverage, ownership,
emulator limitations and the boundary with the current I/O VM deployment.

The [general-purpose PRNG](docs/design/random.md) supplies kernel random values and
KASLR selection without a cryptographic dependency or periodic IRQ work.

[Static tracepoints](docs/design/tracepoints.md) provide opt-in, one-line event
instrumentation with runtime category flags and bounded per-CPU capture.

## Workspace tests

Use the [HypeR workspace](https://github.com/HypeR-Proj/HypeR) to resolve dependencies:

```sh
python3 project.py test kernel
```

Paths in the documentation are relative to the repo-managed workspace root.
Edit this repository and prepare the workspace again before building.

The automatic PR workflow runs eight module shards: quality, scripts, feature
configurations, AArch64 production/compact images, AArch64 self-test + QEMU,
RISC-V QEMU and x86-64 images. All must pass the `module` gate. Run one locally
with `KERNEL_CI_SUITE=aarch64-qemu python3 project.py test kernel`. The QEMU shard
builds its own self-test image; no job depends on another runner's local files.
These kernel-only checks are not part of system verification.
