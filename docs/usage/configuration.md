<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Kernel configuration

Use `make menuconfig` in `kernel/` for interactive editing (`make config` offers
line-oriented input), `make defconfig` to reset to architecture defaults, and
`make olddefconfig` to migrate an existing configuration. Build preparation
creates `.config` only when it is absent or selects a different architecture;
timestamps on Kconfig sources never reset an existing selection. `CONFIG_FILE`
selects an external saved configuration and must already exist when building.

Kconfig selects architecture, address widths, stack sizes, CPU limits, timer
parameters and logging limits. Cargo features select optional functionality.
Each function has one boolean selector; there is no second Kconfig switch for
the same function.

## Production functionality

`KERNEL_FEATURES` is the complete set passed by the kernel Makefile using
`--no-default-features`. Spaces and commas are both accepted:

```sh
make -C kernel image ARCH=aarch64 KERNEL_FEATURES='rtc runtime-symbols'
make -C kernel image ARCH=aarch64 KERNEL_FEATURES=
```

| Feature | Enabled behavior | Disabled behavior |
| --- | --- | --- |
| `rtc` | Discover and read firmware-initialized PL031 or Goldfish clocks | No RTC mappings or driver code; realtime remains Unix epoch plus uptime |
| `runtime-symbols` | Build and embed the compact symbol table; symbolize fatal addresses | Omit table generation, embedding and the runtime decoder; retain registers, raw addresses and bounded frame walking |
| `crash-console` | Interactive allocation-free fatal monitor, UART polling and mapping inspection | Report the crash and halt; no interactive commands or inspection code |
| `pmu-profiling` | AArch64 PMUv3 cycle-overflow sampling and bounded per-CPU call chains | No sampling hook or buffers; profiling queries report unsupported |
| `heap-profiling` | Track allocation traffic and occupancy by heap size class | Basic memory accounting remains; the detailed profiling query reports unsupported |
| `kernel-trace` | Static tracepoint metadata, flag-controlled recording and bounded per-CPU rings | Call sites, field evaluation and recorder storage are omitted |

Make defaults are `rtc runtime-symbols crash-console` on AArch64 and
`rtc runtime-symbols` on RISC-V and x86-64. `heap-profiling`, `pmu-profiling` and `kernel-trace` are off everywhere. See
[PMU recording and export](../../../docs/usage/diagnostics/profiling.md) for limits and authority.
The [tracepoint mechanism](../design/tracepoints.md) exposes kernel-internal
control/read APIs and privileged [Perfetto capture/export](../../../docs/usage/diagnostics/tracing.md) through `perf trace`.
The defaults come from `features.default` in `Cargo.toml`, filtered by that
manifest's architecture availability metadata; Make has no parallel feature list.
Direct Cargo builds default to the AArch64 set; use `--no-default-features`
and an explicit `--features` list to select a different configuration.
The mechanism crates have no implicit defaults: the kernel forwards each chosen
feature to the crate that implements it.

Make resolves feature aliases and quoted Cargo feature lists before deciding
whether to embed runtime symbols or validate a self-test image. Its
`CARGO_FEATURES` input accepts only `--features`/`-F`, `--no-default-features` and
`--all-features`, using root kernel feature names. Use Cargo directly when
selecting qualified features on individual mechanism crates; unsupported Make
arguments fail instead of producing a mismatched image.

Disabling runtime symbols does not strip ELF debug information. Debug-symbol
retention remains controlled by the existing build profile. Crash coordination,
memory protection, capability checks and DMA containment are mandatory mechanisms,
not optional product features.

The retired `CONFIG_CRASH_CONSOLE` and `CONFIG_HEAP_PROFILING` symbols are no
longer accepted. Run `make olddefconfig` to remove retired symbols with explicit
warnings while preserving recognized settings, then select the corresponding
Cargo features. Normal builds and `make defconfig` reject unknown symbols.
The unused `CONFIG_PLATFORM_QEMU` marker was also removed: hardware discovery
and board deployment select devices; this symbol never selected a kernel driver.

## Explicit test permission

`kernel-test` is an independent, default-off permission to compile kernel test
instrumentation. It does not select a test and, on its own, still boots the
production Native init path. Every test hook also requires its individual
selector, including hooks exported by `hyper-core` and `hyper-hal`:

| Selector | Instrumentation |
| --- | --- |
| `kernel-self-test` | Boot-time kernel tests and their private helper APIs |
| `kernel-smmuv3-test` | SMMUv3 device/queue fault fixture; also selects the self-test suite |
| `kernel-vgic-tc-test` | Force the wider virtual GIC compatibility trap |
| `io-dma-revoke-test` | Production-domain DMA revocation fixture and private test mailbox |
| `kernel-stack-audit` | Retired-thread and IRQ stack sampling for acceptance workloads |

No individual selector enables `kernel-test` through Cargo dependencies. A
selector without the umbrella is a build error. All code gates nevertheless
test both features, so removing or bypassing a build-script diagnostic cannot
compile a test hook from the individual selector alone.

```sh
make -C kernel image ARCH=aarch64 \
  CARGO_FEATURES='--features kernel-test,kernel-self-test'
```

The dedicated Make test targets opt into both features. Production image targets
do not add the umbrella. `--all-features` explicitly creates an instrumented test
build and must not be used to produce a deployment image.

Ordinary Rust `cfg(test)` in host test harnesses is separate: it only applies to
host test executables and cannot enable delivered-kernel instrumentation. Host
tests that exercise an opt-in kernel helper must explicitly enable the same
umbrella/selector pair. Architecture cfgs are target facts, not test selectors.
