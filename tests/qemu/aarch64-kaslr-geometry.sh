#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

# Validates the KASLR base without asking the shell to represent an unsigned
# canonical VHE address. POSIX shells use signed arithmetic and dash rejects
# hexadecimal values above INT64_MAX before an expression can reinterpret them.
aarch64_kaslr_geometry_is_valid() {
    kaslr_host_mode=$1
    kaslr_va_bits=$2
    kaslr_kernel_base=$3
    kaslr_offset=$4

    case "$kaslr_va_bits" in
        4[2-8]) ;;
        *) return 1 ;;
    esac

    kaslr_offset_digits=${kaslr_offset#0x}
    case "$kaslr_offset_digits" in
        ''|*[!0-9a-f]*) return 1 ;;
    esac
    [ "${#kaslr_offset_digits}" -le 10 ] || return 1
    kaslr_offset_value=$((0x$kaslr_offset_digits))
    [ $((kaslr_offset_value % 0x200000)) -eq 0 ] || return 1
    [ "$kaslr_offset_value" -lt $((512 * 1024 * 1024 * 1024)) ] || return 1

    case "$kaslr_host_mode" in
        VHE)
            # The final 1 TiB begins at 0xffffff0000000000. Compare its
            # signed-safe low 40 bits after requiring the canonical prefix.
            kaslr_base_digits=${kaslr_kernel_base#0xffffff}
            [ "$kaslr_base_digits" != "$kaslr_kernel_base" ] || return 1
            [ "${#kaslr_base_digits}" -eq 10 ] || return 1
            case "$kaslr_base_digits" in
                *[!0-9a-f]*) return 1 ;;
            esac
            kaslr_base_value=$((0x$kaslr_base_digits))
            kaslr_expected_value=$kaslr_offset_value
            ;;
        nVHE)
            kaslr_base_digits=${kaslr_kernel_base#0x}
            [ "$kaslr_base_digits" != "$kaslr_kernel_base" ] || return 1
            [ "${#kaslr_base_digits}" -le 12 ] || return 1
            case "$kaslr_base_digits" in
                ''|*[!0-9a-f]*) return 1 ;;
            esac
            kaslr_base_value=$((0x$kaslr_base_digits))
            kaslr_expected_value=$(((1 << kaslr_va_bits) - (1 << 40) + kaslr_offset_value))
            ;;
        *)
            return 1
            ;;
    esac

    [ "$kaslr_base_value" -eq "$kaslr_expected_value" ]
}
