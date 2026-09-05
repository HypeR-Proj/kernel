#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

# Acquires the pinned external x86-64 Linux payload used by QEMU integration.
set -eu

version=3.23.5
base=https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/x86_64/netboot-$version
kernel_sha256=a49cf19ae5fa6470b215da782c47aed10c6d395414386b687df772946095241f
initramfs_sha256=23eaf73bdf3122b842834ff29d46ae1ba1c4bffc88d242b1b85f446ad515d598

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
output=$root/target/guest/x86_64
cache=${TMPDIR:-/tmp}/hyper-guest-cache-$version-x86_64
work=$(mktemp -d "${TMPDIR:-/tmp}/hyper-x86_64-guest.XXXXXX")

cleanup() {
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

mkdir -p "$output" "$cache"

fetch() {
    name=$1
    checksum=$2
    destination=$cache/$name
    if [ ! -f "$destination" ] ||
        [ "$(shasum -a 256 "$destination" | awk '{print $1}')" != "$checksum" ]; then
        curl -fL "$base/$name" -o "$destination"
    fi
    actual=$(shasum -a 256 "$destination" | awk '{print $1}')
    if [ "$actual" != "$checksum" ]; then
        echo "$name checksum mismatch: expected $checksum, received $actual" >&2
        exit 1
    fi
}

fetch vmlinuz-virt "$kernel_sha256"
fetch initramfs-virt "$initramfs_sha256"
cp "$cache/vmlinuz-virt" "$output/Image"

mkdir "$work/root"
(
    cd "$work/root"
    gzip -dc "$cache/initramfs-virt" | cpio -id --quiet
    cp "$root/tools/guest/init" init
    chmod 755 init
    find . -print | LC_ALL=C sort | cpio -o -H newc 2>/dev/null |
        gzip -n -9 > "$output/initramfs.cpio.gz.tmp"
)
mv "$output/initramfs.cpio.gz.tmp" "$output/initramfs.cpio.gz"

mkdir -p "$work/vm/kernel" "$work/vm/initramfs"
cp "$root/tools/guest/alpine-x86_64.manifest" "$work/vm/manifest"
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

magic=$(dd if="$output/Image" bs=1 skip=514 count=4 2>/dev/null)
if [ "$magic" != "HdrS" ]; then
    echo "downloaded payload is not an x86 Linux bzImage" >&2
    exit 1
fi

touch "$output/.alpine-$version.stamp"
echo "prepared Alpine $version x86-64 guest payload"
