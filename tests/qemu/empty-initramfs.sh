#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

# Kernel startup still validates a ramfs archive. Supply only a newc trailer;
# standalone mechanism tests must not download a guest or require a Native SDK.
set -eu
[ "$#" -eq 1 ] || { echo "usage: empty-initramfs.sh OUTPUT" >&2; exit 2; }
{
    printf '070701'
    printf '%08x' 1 0 0 0 1 0 0 0 0 0 0 11 0
    printf 'TRAILER!!!\000\000\000\000'
} > "$1"
