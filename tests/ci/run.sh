#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

# Stable local entry points used by GitHub Actions.
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
cd "$root"

usage() {
    echo "usage: kernel/tests/ci/run.sh {quality|scripts|aarch64-build|aarch64-production|aarch64-self-test|aarch64-compact|aarch64-qemu|riscv64-qemu|x86_64-build}" >&2
    exit 2
}

copy_aarch64_artifacts() {
    kind=$1
    output=target/aarch64-unknown-none/kernel
    destination=target/ci/aarch64
    mkdir -p "$destination"
    cp "$output/hyper" "$destination/hyper.$kind"
    cp "$output/hyper.img" "$destination/hyper.$kind.img"
    cp "$output/hyper.kallsyms" "$destination/hyper.$kind.kallsyms"
}

case "${1:-}" in
    quality)
        # Check resolved dependencies and compiler-enforced HAL privacy.
        python3 -B tests/ci/test-hal-boundary.py
        python3 -B tests/ci/hal-boundary.py
        sh tests/ci/test-aarch64-kaslr-geometry.sh
        cargo fmt --all -- --check
        cargo fmt --manifest-path tests/host/Cargo.toml -- --check
        cargo fmt --manifest-path tests/concurrency/Cargo.toml -- --check
        cargo fmt --manifest-path tools/kconfig/Cargo.toml -- --check
        cargo fmt --manifest-path tools/kallsyms/Cargo.toml -- --check
        make test ARCH=aarch64
        ;;
    scripts)
        command -v shellcheck >/dev/null 2>&1 || {
            echo "shellcheck is required for the script-quality suite" >&2
            exit 2
        }
        find tests tools/guest -type f -name '*.sh' -print0 |
            xargs -0 shellcheck --severity=warning
        ;;
    aarch64-build)
        for variant in production self-test compact; do
            sh tests/ci/run.sh "aarch64-$variant"
        done
        ;;
    aarch64-production)
        sh tests/ci/check-aarch64-address-configs.sh
        make check ARCH=aarch64
        make release ARCH=aarch64
        make test-image ARCH=aarch64
        copy_aarch64_artifacts production
        cp target/aarch64-unknown-none/kernel/hyper.stripped \
            target/ci/aarch64/hyper.production.stripped
        cp target/aarch64-unknown-none/kernel/hyper.stripped.img \
            target/ci/aarch64/hyper.production.stripped.img
        ;;
    aarch64-self-test)
        make image ARCH=aarch64 CARGO_FEATURES="--features kernel-self-test"
        make test-image ARCH=aarch64
        copy_aarch64_artifacts self-test
        ;;
    aarch64-compact)
        mkdir -p target/ci/aarch64
        compact_config=target/ci/aarch64/compact.config
        sed \
            -e 's/^CONFIG_ARM64_VA_BITS=.*/CONFIG_ARM64_VA_BITS=42/' \
            -e 's/^CONFIG_ARM64_PA_BITS=.*/CONFIG_ARM64_PA_BITS=40/' \
            configs/qemu_aarch64_defconfig >"$compact_config"
        make image ARCH=aarch64 CONFIG_FILE="$compact_config" \
            CARGO_FEATURES="--features kernel-self-test"
        make test-image ARCH=aarch64
        copy_aarch64_artifacts compact
        ;;
    aarch64-qemu)
        test_image=${AARCH64_TEST_IMAGE:-target/ci/aarch64/hyper.self-test.img}
        test -f "$test_image" || {
            echo "missing AArch64 self-test image: $test_image" >&2
            exit 2
        }
        QEMU_CPUS=${QEMU_CPUS:-4} \
            QEMU_BOOT_TIMEOUT_SECONDS=${QEMU_BOOT_TIMEOUT_SECONDS:-120} \
            sh tests/qemu/verify-smp.sh \
            "${QEMU:-qemu-system-aarch64}" \
            "$test_image" \
            "${QEMU_CPU:-max}" \
            "${QEMU_MEMORY:-1G}" \
            "${QEMU_BOOTARGS:-earlycon=pl011,mmio32,0x09000000}"
        ;;
    riscv64-qemu)
        QEMU_TEST_LOG=${QEMU_TEST_LOG:-target/ci/logs/riscv64.log} \
            QEMU_BOOT_TIMEOUT_SECONDS=${QEMU_BOOT_TIMEOUT_SECONDS:-180} \
            make test-qemu ARCH=riscv64 QEMU_CPU=rv64 QEMU_CPUS=4
        ;;
    x86_64-build)
        make check ARCH=x86_64
        make release ARCH=x86_64
        make test-image ARCH=x86_64
        make image ARCH=x86_64 CARGO_FEATURES="--features kernel-self-test"
        make test-image ARCH=x86_64 CARGO_FEATURES="--features kernel-self-test"
        ;;
    *)
        usage
        ;;
esac
