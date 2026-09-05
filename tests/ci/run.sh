#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

# Stable local entry points used by GitHub Actions.
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
cd "$root"

usage() {
    echo "usage: kernel/tests/ci/run.sh {quality|scripts|aarch64-build|aarch64-qemu|riscv64-qemu|x86_64-build}" >&2
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

prepare_aarch64_guest() {
    stamp=target/guest/aarch64/.alpine-3.23.5.stamp
    for input in \
        tools/guest/fetch-alpine-aarch64.sh \
        tools/guest/alpine-aarch64.manifest \
        tools/guest/init \
        tools/guest/boot.conf; do
        if [ ! -f "$stamp" ] || find "$input" -newer "$stamp" -print | grep -q .; then
            sh tools/guest/fetch-alpine-aarch64.sh
            return
        fi
    done
}

case "${1:-}" in
    quality)
        command -v rg >/dev/null 2>&1 || {
            echo "ripgrep is required for the source-quality suite" >&2
            exit 2
        }
        sh tests/ci/test-boot-stack-contract.sh
        sh tests/ci/check-boot-stack-contract.sh
        sh tests/ci/test-arch-facades.sh
        sh tests/ci/check-arch-facades.sh
        sh tests/ci/test-irq-registration-contract.sh
        sh tests/ci/check-irq-registration-contract.sh
        sh tests/ci/test-irq-transition-contract.sh
        sh tests/ci/check-irq-transition-contract.sh
        sh tests/ci/test-boot-lifecycle-contract.sh
        sh tests/ci/check-boot-lifecycle-contract.sh
        sh tests/ci/test-local-irq-lifecycle-contract.sh
        sh tests/ci/check-local-irq-lifecycle-contract.sh
        sh tests/ci/test-cross-call-publisher-pinning-contract.sh
        sh tests/ci/check-cross-call-publisher-pinning-contract.sh
        sh tests/ci/test-arch-boundaries.sh
        sh tests/ci/check-arch-boundaries.sh
        sh tests/ci/test-reschedule-publication-contract.sh
        sh tests/ci/check-reschedule-publication-contract.sh
        sh tests/ci/test-thread-migration-context-contract.sh
        sh tests/ci/check-thread-migration-context-contract.sh
        sh tests/ci/test-thread-retirement-contract.sh
        sh tests/ci/check-thread-retirement-contract.sh
        sh tests/ci/test-thread-resource-alias-contract.sh
        sh tests/ci/check-thread-resource-alias-contract.sh
        sh tests/ci/test-scheduler-cpu-ownership-contract.sh
        sh tests/ci/check-scheduler-cpu-ownership-contract.sh
        sh tests/ci/test-scheduler-thread-table-contract.sh
        sh tests/ci/check-scheduler-thread-table-contract.sh
        sh tests/ci/test-x86-stage1-shootdown-contract.sh
        sh tests/ci/check-x86-stage1-shootdown-contract.sh
        sh tests/ci/test-secondary-handoff-contract.sh
        sh tests/ci/check-secondary-handoff-contract.sh
        sh tests/ci/test-riscv-guest-frame-contract.sh
        sh tests/ci/check-riscv-guest-frame-contract.sh
        sh tests/ci/test-native-user-boundary.sh
        sh tests/ci/check-native-user-boundary.sh
        sh tests/ci/test-vcpu-transition-contract.sh
        sh tests/ci/check-vcpu-transition-contract.sh
        sh tests/ci/test-aarch64-guest-unwind-contract.sh
        sh tests/ci/check-aarch64-guest-unwind-contract.sh
        sh tests/ci/test-vcpu-interrupt-publication-contract.sh
        sh tests/ci/check-vcpu-interrupt-publication-contract.sh
        sh tests/ci/test-vm-retirement-contract.sh
        sh tests/ci/check-vm-retirement-contract.sh
        sh tests/ci/test-guest-residency-contract.sh
        sh tests/ci/check-guest-residency-contract.sh
        sh tests/ci/test-allocator-invariant-contract.sh
        sh tests/ci/check-allocator-invariant-contract.sh
        sh tests/ci/test-allocator-cache-contract.sh
        sh tests/ci/check-allocator-cache-contract.sh
        sh tests/ci/test-deferred-log-contract.sh
        sh tests/ci/check-deferred-log-contract.sh
        sh tests/ci/test-aarch64-kaslr-geometry.sh
        cargo fmt --all -- --check
        cargo fmt --manifest-path tests/host/Cargo.toml -- --check
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
        sh tests/ci/check-aarch64-address-configs.sh
        make check ARCH=aarch64
        make release ARCH=aarch64
        make test-image ARCH=aarch64
        copy_aarch64_artifacts production
        cp target/aarch64-unknown-none/kernel/hyper.stripped \
            target/ci/aarch64/hyper.production.stripped
        cp target/aarch64-unknown-none/kernel/hyper.stripped.img \
            target/ci/aarch64/hyper.production.stripped.img

        make image ARCH=aarch64 CARGO_FEATURES="--features kernel-self-test"
        make test-image ARCH=aarch64
        copy_aarch64_artifacts self-test

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
        prepare_aarch64_guest
        QEMU_CPUS=${QEMU_CPUS:-4} \
            QEMU_BOOT_TIMEOUT_SECONDS=${QEMU_BOOT_TIMEOUT_SECONDS:-120} \
            sh tests/qemu/verify-smp.sh \
            "${QEMU:-qemu-system-aarch64}" \
            "$test_image" \
            target/guest/aarch64/hypervisor-initrd.cpio \
            "${QEMU_CPU:-cortex-a72}" \
            "${QEMU_MEMORY:-512M}" \
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
        ;;
    *)
        usage
        ;;
esac
