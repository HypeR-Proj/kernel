#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

# Exercises the complete AArch64 runtime contract under QEMU.
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
# shellcheck source=tests/qemu/aarch64-kaslr-geometry.sh
. "$script_dir/aarch64-kaslr-geometry.sh"

if [ "$#" -ne 5 ]; then
    echo "usage: verify-smp.sh QEMU IMAGE CPU MEMORY BOOTARGS" >&2
    exit 2
fi

qemu=$1
image=$2
cpu=$3
memory=$4
bootargs=$5
cpus=${QEMU_CPUS:-4}
timeout_seconds=${QEMU_BOOT_TIMEOUT_SECONDS:-120}

case "$cpu" in
    cortex-a57|cortex-a72)
        default_host_mode=nVHE
        default_atomic_backend=LL/SC
        ;;
    max)
        default_host_mode=VHE
        default_atomic_backend=LSE
        ;;
    *)
        default_host_mode='\(nVHE\|VHE\)'
        default_atomic_backend='\(LL/SC\|LSE\)'
        ;;
esac
host_mode=${QEMU_EXPECT_HOST_MODE:-$default_host_mode}
atomic_backend=${QEMU_EXPECT_ATOMIC_BACKEND:-$default_atomic_backend}
va_bits=${QEMU_EXPECT_VA_BITS:-48}
pa_bits=${QEMU_EXPECT_PA_BITS:-'[0-9][0-9]'}

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
case "$va_bits" in
    4[2-8]) ;;
    *)
        echo "QEMU_EXPECT_VA_BITS must be in 42..48" >&2
        exit 2
        ;;
esac

temp=$(mktemp -d -t hyper-qemu-smp.XXXXXX)
if [ -n "${QEMU_TEST_LOG:-}" ]; then
    mkdir -p "$(dirname "$QEMU_TEST_LOG")"
    log=$QEMU_TEST_LOG
    : > "$log"
else
    log=$temp/output.log
fi
pid=
attempt_limit=$((timeout_seconds * 10))


cleanup() {
    if [ -n "$pid" ]; then
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
        fi
        wait "$pid" 2>/dev/null || true
    fi
    rm -rf "$temp"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

kaslr_geometry_is_valid() {
    kaslr_base=$(sed -n 's/.*randomized kernel base \(0x[0-9a-f][0-9a-f]*\),.*/\1/p' "$log" | tail -n 1)
    kaslr_offset=$(sed -n 's/.*KASLR offset \(0x[0-9a-f][0-9a-f]*\).*/\1/p' "$log" | tail -n 1)
    [ -n "$kaslr_base" ] && [ -n "$kaslr_offset" ] || return 1
    actual_host_mode=$(
        sed -n 's/.*HypeR: AArch64 host execution mode: //p' "$log" |
            tr -d '\r' |
            tail -n 1
    )
    aarch64_kaslr_geometry_is_valid \
        "$actual_host_mode" "$va_bits" "$kaslr_base" "$kaslr_offset"
}

reschedule_ipi_proof_is_valid() {
    if [ "$cpus" -gt 1 ]; then
        grep -q 'HypeR test: targeted reschedule IPI delivery and EOI passed' "$log"
    else
        grep -q 'HypeR test: targeted reschedule IPI skipped (one CPU online)' "$log"
    fi
}

runtime_contract_is_ready() {
    grep -q '<6>\[[ 0-9]\{5,\}\.[0-9]\{6\}\] HypeR: early console initialized' "$log" &&
        grep -q "HypeR: atomic RMW backend: $atomic_backend" "$log" &&
        grep -q "HypeR: AArch64 address space: $va_bits-bit VA/4 levels, $pa_bits-bit PA (CPU [0-9][0-9]-bit), 39-bit IPA/3 levels" "$log" &&
        grep -q 'HypeR: scheduler active on bootstrap thread 0' "$log" &&
        grep -q 'HypeR test: scheduler ready/wait queues and sleeping sync passed' "$log" &&
        if [ "$cpus" -ge 4 ]; then
            grep -q 'HypeR test: independent CPU wait/wake progress passed (256 round trips)' "$log"
        else
            true
        fi &&
        if [ "$cpus" -gt 1 ]; then
            grep -q 'HypeR test: cross-CPU thread migration passed' "$log"
        else
            grep -q 'HypeR test: cross-CPU thread migration skipped (one CPU online)' "$log"
        fi &&
        grep -q 'HypeR test: guarded thread, IRQ, and emergency stacks passed' "$log" &&
        grep -q 'HypeR test: fatal-path readiness contract passed' "$log" &&
        grep -q 'HypeR test: Native syscall validation passed' "$log" &&
        grep -q 'HypeR test: Channel Process and user-copy transactions passed' "$log" &&
        grep -q 'HypeR test: AArch64 EL0 syscall and fault containment passed' "$log" &&
        reschedule_ipi_proof_is_valid &&
        grep -q 'HypeR test: checked stage-2 guest-memory copies passed' "$log" &&
        grep -q 'HypeR test: checked application-memory copies passed' "$log" &&
        grep -q 'HypeR: kallsyms resolved hyper_kallsyms_lookup at 0x[0-9a-f][0-9a-f]*' "$log" &&
        grep -q 'HypeR: kernel log ring: 65536 bytes' "$log" &&
        grep -q 'HypeR: CPU power interface version .*: on=true, off=true, suspend=true, reset=true' "$log" &&
        grep -q 'HypeR: vGICv3 active with [1-9][0-9]* LRs, [5-8] priority bits, [5-7] preemption bits, \(16\|24\) INTID bits, maintenance VIRQ [0-9][0-9]*' "$log" &&
        grep -q 'HypeR: architectural timer: host INTID 26, guest INTID 27, [1-9][0-9]* Hz tick from a [1-9][0-9]* Hz counter' "$log" &&
        grep -q 'HypeR: guest architectural timer mapped to host VIRQ [0-9][0-9]*' "$log" &&
        grep -q 'HypeR: monotonic clocksource active at [1-9][0-9]* Hz' "$log" &&
        grep -q 'HypeR: virtual architected timer injection validated' "$log" &&
        grep -q 'HypeR: guest synchronous trap and vSysReg emulation validated' "$log" &&
        grep -q 'HypeR: platform bus: .* bound, .* unmatched, .* deferred, .* failed' "$log" &&
        grep -q "HypeR: SMP online: $cpus/$cpus discovered CPUs" "$log" &&
        grep -q "HypeR: heap caches: $cpus CPUs, [0-9][0-9]* objects," "$log" &&
        grep -q 'HypeR: randomized kernel base 0x[0-9a-f][0-9a-f]*, KASLR offset 0x[0-9a-f][0-9a-f]*' "$log" &&
        grep -q 'HypeR: transition identity mappings retired' "$log" &&
        grep -q "HypeR: AArch64 host execution mode: $host_mode" "$log" &&
        grep -Eq 'HypeR: AArch64 execution protection: (XN|PXN/UXN), WXN=on' "$log" &&
        grep -q 'HypeR test: AArch64 guest-entry IRQ mask contract passed' "$log" &&
        grep -q "HypeR: periodic timer IRQs active on $cpus CPUs" "$log" &&
        grep -q 'HypeR test: kernel self-tests completed' "$log"
}

initrd=$temp/empty.cpio
sh "$(dirname "$0")/empty-initramfs.sh" "$initrd"

"$qemu" \
    -machine virt,virtualization=on,gic-version=3,dtb-randomness=on \
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
    -kernel "$image" </dev/null >"$log" 2>&1 &
pid=$!

attempt=0
while [ "$attempt" -lt "$attempt_limit" ]; do
    if grep -Eq '<0>\[[ 0-9]+\.[0-9]{6}\].*(PANIC|BUG)|HypeR crash monitor|allocator invariant failure' "$log"; then
        cat "$log" >&2
        echo "HypeR reported a fatal failure during the AArch64 integration test" >&2
        exit 1
    fi
    if runtime_contract_is_ready; then
        if ! kaslr_geometry_is_valid; then
            cat "$log" >&2
            echo "invalid AArch64 KASLR geometry" >&2
            exit 1
        fi
        echo "verified $cpus-CPU AArch64 $host_mode/$atomic_backend kernel self-tests on QEMU CPU $cpu"
        exit 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
        cat "$log" >&2
        echo "QEMU exited before the AArch64 runtime contract completed" >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done

cat "$log" >&2
echo "timed out after ${timeout_seconds}s waiting for the AArch64 runtime contract" >&2
exit 1
