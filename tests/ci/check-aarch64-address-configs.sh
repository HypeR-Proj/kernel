#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

# Compile representative four-level host and three-level guest address widths.
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
base=$root/configs/qemu_aarch64_defconfig
temporary=$(mktemp -d -t hyper-aarch64-address.XXXXXX)
trap 'rm -rf "$temporary"' EXIT INT TERM

check_configuration() {
    va_bits=$1
    pa_bits=$2
    ipa_bits=$3
    configuration=$temporary/va${va_bits}-pa${pa_bits}-ipa${ipa_bits}.config

    sed \
        -e "s/^CONFIG_ARM64_VA_BITS=.*/CONFIG_ARM64_VA_BITS=$va_bits/" \
        -e "s/^CONFIG_ARM64_PA_BITS=.*/CONFIG_ARM64_PA_BITS=$pa_bits/" \
        -e "s/^CONFIG_ARM64_IPA_BITS=.*/CONFIG_ARM64_IPA_BITS=$ipa_bits/" \
        "$base" >"$configuration"

    HYPER_CONFIG=$configuration cargo check --lib --bins --target aarch64-unknown-none
}

check_configuration 42 36 32
check_configuration 44 42 36
check_configuration 48 48 39
