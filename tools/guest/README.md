<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# External Linux guest test payload

`make guest-assets` downloads the checksum-pinned Alpine Linux guest selected
by `ARCH`: 3.23.5 for AArch64 and x86-64, or 3.24.1 for RISC-V 64-bit. It
packages it as a versioned inner VM CPIO and then builds the outer U-Boot
ramdisk described in `docs/vm-bundle.md`. Generated payload files are ignored
by Git and are not Apache-2.0 project source.

Each script verifies SHA-256 checksums before extracting the standard
Linux kernel payload (`Image` on Arm/RISC-V and bzImage on x86-64). It replaces
the distribution initramfs entry point with the deterministic integration
`/init` from `tools/guest`.
The QEMU tests pass `hypervisor-initrd.cpio` with `-initrd`; HypeR itself no
longer embeds a Linux binary. All generated files are placed under
`target/guest/`.

The Linux kernel is licensed under GPL-2.0-only. Alpine packages have their own
licenses. Do not redistribute generated payloads as part of an Apache-2.0-only
source release; retain upstream license and corresponding-source obligations
if binary artifacts containing them are distributed.
