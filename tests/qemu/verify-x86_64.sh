#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

set -eu

qemu=$1
image=$2
initrd=$(mktemp)
cpu=$3
memory=$4
bootargs=$5
output=$(mktemp)
dtb=$(mktemp)
accel=${QEMU_ACCEL:-tcg}
cleanup() {
    if [ -n "${qemu_pid:-}" ]; then
        kill "$qemu_pid" 2>/dev/null || true
        wait "$qemu_pid" 2>/dev/null || true
    fi
    rm -f "$output" "$dtb" "$initrd"
}
trap cleanup EXIT INT TERM

sh "$(dirname "$0")/empty-initramfs.sh" "$initrd"
dtc -q -I dts -O dtb -o "$dtb" tests/qemu/x86_64-host.dts
"$qemu" \
    -machine "q35,accel=$accel" \
    -cpu "$cpu" \
    -smp 4 \
    -m "$memory" \
    -nodefaults \
    -display none \
    -serial stdio \
    -monitor none \
    -no-reboot \
    -append "$bootargs" \
    -initrd "$initrd" \
    -dtb "$dtb" \
    -kernel "$image" >"$output" 2>&1 &
qemu_pid=$!

deadline=$(( $(date +%s) + ${QEMU_BOOT_TIMEOUT_SECONDS:-180} ))
while kill -0 "$qemu_pid" 2>/dev/null; do
    if grep -q 'HypeR test: cross-CPU thread migration passed' "$output" &&
        grep -q "HypeR test: kernel self-tests completed" "$output"; then
        cat "$output"
        exit 0
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
        cat "$output"
        echo "timed out waiting for the x86-64 smoke-test completion marker" >&2
        exit 1
    fi
    sleep 1
done
cat "$output"
echo "QEMU exited before the x86-64 kernel self-tests completed" >&2
exit 1
