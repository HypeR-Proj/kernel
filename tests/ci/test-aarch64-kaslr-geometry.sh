#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
# shellcheck source=tests/qemu/aarch64-kaslr-geometry.sh
. "$root/tests/qemu/aarch64-kaslr-geometry.sh"

expect_valid() {
    if ! aarch64_kaslr_geometry_is_valid "$@"; then
        echo "expected valid AArch64 KASLR geometry: $*" >&2
        exit 1
    fi
}

expect_invalid() {
    if aarch64_kaslr_geometry_is_valid "$@"; then
        echo "expected invalid AArch64 KASLR geometry: $*" >&2
        exit 1
    fi
}

expect_valid VHE 48 0xffffff48e0600000 0x48e0600000
expect_valid VHE 42 0xffffff0000200000 0x200000
expect_valid nVHE 48 0xff0000200000 0x200000
expect_valid nVHE 42 0x30000200000 0x200000

expect_invalid VHE 48 0xfffffe48e0600000 0x48e0600000
expect_invalid VHE 48 0xffffff48e0400000 0x48e0600000
expect_invalid nVHE 48 0xff0000400000 0x200000
expect_invalid nVHE 42 0x30000000001 0x1
expect_invalid VHE 48 0xffffff8000000000 0x8000000000

echo "AArch64 KASLR geometry parser tests passed"
