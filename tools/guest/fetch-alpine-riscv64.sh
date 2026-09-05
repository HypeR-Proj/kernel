#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

# Acquires the pinned external RISC-V Linux payload used by QEMU integration.
set -eu

version=3.24.1
archive=alpine-uboot-$version-riscv64.tar.gz
base=https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/riscv64
archive_sha256=9c364c26b233a8f7e2a56647cdd5e346b09c4070879fe8ea73011c130a92f023
kernel_sha256=dc2f8add0afaecba3b75a0157d82317a7f09b555cad1c233f68ce82697255d76
initramfs_sha256=d63639ef0e8c9cff9d835bae4ce75ce52552af710016debe9721cad7aaf74bd1

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
output=$root/target/guest/riscv64
cache=${TMPDIR:-/tmp}/hyper-guest-cache-$version-riscv64
work=$(mktemp -d "${TMPDIR:-/tmp}/hyper-riscv64-guest.XXXXXX")

cleanup() {
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

mkdir -p "$output" "$cache" "$work/archive"
if [ ! -f "$cache/$archive" ] ||
    [ "$(shasum -a 256 "$cache/$archive" | awk '{print $1}')" != "$archive_sha256" ]; then
    curl -fL "$base/$archive" -o "$cache/$archive"
fi
actual=$(shasum -a 256 "$cache/$archive" | awk '{print $1}')
if [ "$actual" != "$archive_sha256" ]; then
    echo "$archive checksum mismatch: expected $archive_sha256, received $actual" >&2
    exit 1
fi

tar -xzf "$cache/$archive" -C "$work/archive" \
    ./boot/vmlinuz-lts ./boot/initramfs-lts
for record in \
    "vmlinuz-lts:$kernel_sha256" \
    "initramfs-lts:$initramfs_sha256"; do
    name=${record%%:*}
    expected=${record#*:}
    actual=$(shasum -a 256 "$work/archive/boot/$name" | awk '{print $1}')
    if [ "$actual" != "$expected" ]; then
        echo "$name checksum mismatch: expected $expected, received $actual" >&2
        exit 1
    fi
done

gzip -dc "$work/archive/boot/vmlinuz-lts" > "$output/Image.tmp"
mv "$output/Image.tmp" "$output/Image"

mkdir "$work/root"
(
    cd "$work/root"
    gzip -dc "$work/archive/boot/initramfs-lts" | cpio -id --quiet
    cp "$root/tools/guest/init" init
    chmod 755 init
    find . -print | LC_ALL=C sort | cpio -o -H newc 2>/dev/null |
        gzip -n -9 > "$output/initramfs.cpio.gz.tmp"
)
mv "$output/initramfs.cpio.gz.tmp" "$output/initramfs.cpio.gz"

mkdir -p "$work/vm/kernel" "$work/vm/initramfs"
cp "$root/tools/guest/alpine-riscv64.manifest" "$work/vm/manifest"
cp "$output/Image" "$work/vm/kernel/Image"
cp "$output/initramfs.cpio.gz" "$work/vm/initramfs/initramfs.cpio.gz"
(
    cd "$work/vm"
    find manifest kernel initramfs -print | LC_ALL=C sort |
        cpio -o -H newc 2>/dev/null > "$output/alpine.cpio.tmp"
)
mv "$output/alpine.cpio.tmp" "$output/alpine.cpio"

mkdir -p "$work/boot/hypervisor/vms"
cp "$root/tools/guest/boot.conf" "$work/boot/hypervisor/boot.conf"
cp "$output/alpine.cpio" "$work/boot/hypervisor/vms/alpine.cpio"
(
    cd "$work/boot"
    find hypervisor -print | LC_ALL=C sort |
        cpio -o -H newc 2>/dev/null > "$output/hypervisor-initrd.cpio.tmp"
)
mv "$output/hypervisor-initrd.cpio.tmp" "$output/hypervisor-initrd.cpio"

magic=$(dd if="$output/Image" bs=1 skip=56 count=4 2>/dev/null | od -An -tx1 | tr -d ' \n')
if [ "$magic" != "52534305" ]; then
    echo "extracted payload is not a RISC-V Linux Image" >&2
    exit 1
fi

touch "$output/.alpine-$version.stamp"
echo "prepared Alpine $version RISC-V guest payload"
