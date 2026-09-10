#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

# Boots the RISC-V host and requires standalone kernel mechanism tests.
set -eu

if [ "$#" -ne 5 ]; then
    echo "usage: verify-riscv64.sh QEMU IMAGE CPU MEMORY BOOTARGS" >&2
    exit 2
fi

qemu=$1
image=$2
cpu=$3
memory=$4
bootargs=$5
cpus=${QEMU_CPUS:-4}
timeout_seconds=${QEMU_BOOT_TIMEOUT_SECONDS:-180}
temp=$(mktemp -d -t hyper-qemu-riscv64.XXXXXX)
if [ -n "${QEMU_TEST_LOG:-}" ]; then
    mkdir -p "$(dirname "$QEMU_TEST_LOG")"
    log=$QEMU_TEST_LOG
    : > "$log"
else
    log=$temp/output.log
fi
pid=

case "$cpus" in
    ''|*[!0-9]*|0)
        echo "QEMU_CPUS must be a positive integer" >&2
        exit 2
        ;;
esac
case "$timeout_seconds" in
    ''|*[!0-9]*|0)
        echo "QEMU_BOOT_TIMEOUT_SECONDS must be a positive integer" >&2
        exit 2
        ;;
esac
attempt_limit=$((timeout_seconds * 10))

magic=$(dd if="$image" bs=1 skip=56 count=4 2>/dev/null | od -An -tx1 | tr -d ' \n')
if [ "$magic" != "52534305" ]; then
    echo "HypeR image does not contain a RISC-V Linux Image header" >&2
    exit 1
fi

cleanup() {
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
    fi
    if [ -n "$pid" ]; then
        wait "$pid" 2>/dev/null || true
    fi
    rm -rf "$temp"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

initrd=$temp/empty.cpio
sh "$(dirname "$0")/empty-initramfs.sh" "$initrd"

"$qemu" \
    -machine virt \
    -accel tcg,thread=multi \
    -cpu "$cpu" \
    -smp "$cpus" \
    -m "$memory" \
    -nodefaults \
    -display none \
    -serial stdio \
    -monitor none \
    -no-reboot \
    -append "$bootargs" \
    -initrd "$initrd" \
    -kernel "$image" >"$log" 2>&1 &
pid=$!

attempt=0
while [ "$attempt" -lt "$attempt_limit" ]; do
    if grep -Eq '<0>\[[ 0-9]+\.[0-9]{6}\].*(PANIC|BUG)|HypeR crash monitor|allocator invariant failure' "$log"; then
        cat "$log" >&2
        echo "HypeR reported a fatal failure during the RISC-V integration test" >&2
        exit 1
    fi
    if grep -q 'HypeR: transition identity mappings retired' "$log" &&
        grep -q 'HypeR test: cross-CPU thread migration passed' "$log" &&
        grep -q 'HypeR test: Native register isolation passed' "$log" &&
        grep -q 'HypeR test: Native supervisor mapping and CSR faults contained' "$log" &&
        grep -q 'HypeR test: kernel self-tests completed' "$log"; then
        echo "verified RISC-V kernel startup and self-tests on QEMU CPU $cpu"
        exit 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
        cat "$log" >&2
        echo "QEMU exited before the RISC-V kernel self-tests completed" >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done

cat "$log" >&2
echo "timed out after ${timeout_seconds}s waiting for the RISC-V kernel self-tests" >&2
exit 1
