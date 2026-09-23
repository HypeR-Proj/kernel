<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# External Linux guest test payload

`make guest-assets` downloads checksum-pinned Alpine Linux kernels and base
root filesystems: 3.23.5 for AArch64/x86-64 and 3.24.1 for RISC-V. The userspace
is Alpine minirootfs, with musl, BusyBox applet links, the package database and
`apk`; it is no longer the netboot rescue shell. Network access still depends
on the devices supplied to the guest; this does not add a guest network backend.

The scripts retain matching netboot modules. AArch64 additionally extracts the
virtio-scsi/SCSI/ext4 dependency closure from the pinned distribution modloop
using `unsquashfs` (`squashfs-tools` on Linux, `squashfs` on Homebrew).
No distribution source or generated binary is committed to this repository.

Standalone Native integration uses the complete userspace in an initramfs.
Board deployment generates an ext4 root disk using e2fsprogs (`mke2fs` and
`debugfs`), with its size taken from the board JSON's VM volume. The board FIT
passes `hyper.root=/dev/sda`; `/init` loads the disk modules, mounts that root
read/write, and uses `switch_root`. Its initial RAM root is then discarded.
The root disk is supplied through I/O VM virtio-scsi, not a direct guest device.
The configuration partition holds the guest FIT, while the opaque per-VM
partition holds the ext4 disk. The HypeR bootstrap contains only the I/O VM FIT.

`/init` prepares the root filesystem and hands PID 1 to BusyBox init, which
starts an interactive root shell with a controlling terminal, reaps children
and restarts the shell on exit. This is a development image, without a login service or
OpenRC service setup. `make test-alpine-rootfs` verifies a 1 MiB file write,
flush and checksum after VM stop/start using a disposable board disk.

Alpine images default to 256 MiB of RAM; override `NATIVE_GUEST_MEMORY_BYTES`
when building to select another capacity. AArch64 images default to two vCPUs
on both QEMU and Pi 5. Override
`NATIVE_GUEST_VCPUS` when building to select another topology; the dedicated
`make test-guest-smp` fixture still defaults to four vCPUs. RISC-V guests
remain single-vCPU.

Use ordinary `poweroff` and `reboot` commands. BusyBox init runs the shutdown
actions to sync and unmount filesystems (remounting busy filesystems read-only)
before the kernel power operation. The QEMU SMP test uses these same commands
to cover the userspace shutdown path as well as PSCI.

Generated payloads live under `kernel/target/guest/<arch>/`. Run
`make clean-guest-assets ARCH=aarch64` to remove generated kernel/rootfs
inputs; verified download caches can be reused. Plain AArch64 `make` rebuilds
the kernel/bootstrap and preserves an existing board disk. Use `make rebuild`
to repack guest payloads into that disk, resetting its persistent contents.
`make run` only launches existing artifacts; it does not regenerate them.

The Linux kernel is licensed under GPL-2.0-only. Alpine packages have their own
licenses. Do not redistribute generated payloads as part of an Apache-2.0-only
source release; retain upstream license and corresponding-source obligations
if binary artifacts containing them are distributed.
