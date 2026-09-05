#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

# Acquires the pinned external Linux guest payload used by QEMU integration.
set -eu

version=3.23.5
base=https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/aarch64/netboot-$version
kernel_sha256=1a2fa67cb25a2fa9065818712d50d0d543526818b3c6b43695e54deaca33d66d
initramfs_sha256=df5281b4c36f812d0507e219e31a8a7482e0b4175097e292b75c7872c441295c
zboot_payload_offset=51832

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
output=$root/target/guest/aarch64
cache=${TMPDIR:-/tmp}/hyper-guest-cache-$version
work=$(mktemp -d "${TMPDIR:-/tmp}/hyper-guest.XXXXXX")

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

# Alpine packages AArch64 as an EFI zboot image. Its pinned gzip payload starts
# at the offset above and expands to the standard uncompressed Linux Image.
status=0
dd if="$cache/vmlinuz-virt" bs=1 skip="$zboot_payload_offset" 2>/dev/null |
    gzip -dc 2>"$work/zboot-gzip.stderr" > "$output/Image.tmp" || status=$?
if [ "$status" -ne 2 ] && [ "$status" -ne 0 ]; then
    cat "$work/zboot-gzip.stderr" >&2
    exit "$status"
fi
mv "$output/Image.tmp" "$output/Image"

mkdir "$work/root"
(
    cd "$work/root"
    gzip -dc "$cache/initramfs-virt" | cpio -id --quiet
    cp "$root/tools/guest/init" init
    chmod 755 init
    find . -print | LC_ALL=C sort | cpio -o -H newc 2>/dev/null | gzip -n -9 > "$output/initramfs.cpio.gz.tmp"
)
mv "$output/initramfs.cpio.gz.tmp" "$output/initramfs.cpio.gz"

mkdir -p "$work/vm/kernel" "$work/vm/initramfs"
cp "$root/tools/guest/alpine-aarch64.manifest" "$work/vm/manifest"
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

image_magic=$(dd if="$output/Image" bs=1 skip=56 count=4 2>/dev/null)
if [ "$image_magic" != "ARMd" ]; then
    echo "extracted payload is not an AArch64 Linux Image" >&2
    exit 1
fi

touch "$output/.alpine-$version.stamp"
echo "prepared Alpine $version AArch64 guest payload"
