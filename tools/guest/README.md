<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# External Linux guest test payload

`make guest-assets` downloads the checksum-pinned Alpine Linux guest selected
by `ARCH`: 3.23.5 for AArch64 and x86-64, or 3.24.1 for RISC-V 64-bit. It
prepares the kernel and initramfs inputs for userspace image tooling. The root build composes
a guest FIT and places it in the Native system initramfs. Generated payload files are ignored
by Git and are not Apache-2.0 project source.

Each script verifies SHA-256 checksums before extracting the standard
Linux kernel payload (`Image` on Arm/RISC-V and bzImage on x86-64). It replaces
the distribution initramfs entry point with the deterministic integration
`/init` from `tools/guest`.
Native integration tests pass the system initramfs and launch guests through
userspace VMM tools. Standalone kernel tests use an empty ramfs and do not
download guest payloads. All generated files are placed under
`target/guest/`.

The Linux kernel is licensed under GPL-2.0-only. Alpine packages have their own
licenses. Do not redistribute generated payloads as part of an Apache-2.0-only
source release; retain upstream license and corresponding-source obligations
if binary artifacts containing them are distributed.
