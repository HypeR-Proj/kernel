#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

# Exercises the complete AArch64 runtime contract under QEMU.
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
# shellcheck source=tests/qemu/aarch64-kaslr-geometry.sh
. "$script_dir/aarch64-kaslr-geometry.sh"

if [ "$#" -ne 6 ]; then
    echo "usage: verify-smp.sh QEMU IMAGE INITRD CPU MEMORY BOOTARGS" >&2
    exit 2
fi

qemu=$1
image=$2
initrd=$3
cpu=$4
memory=$5
bootargs=$6
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
input=$temp/input
pid=
input_sent=false
attempt_limit=$((timeout_seconds * 10))

mkfifo "$input"
exec 3<>"$input"

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

secondary_cpus_online() {
    cpu_id=1
    while [ "$cpu_id" -lt "$cpus" ]; do
        hardware_id=$(printf '%x' "$cpu_id")
        grep -q "HypeR: CPU $cpu_id online, hardware ID 0x$hardware_id; entering idle" "$log" ||
            return 1
        cpu_id=$((cpu_id + 1))
    done
}

demand_paging_is_lazy() {
    values=$(sed -n 's/.*HypeR: guest demand paging: \([0-9][0-9]*\)\/\([0-9][0-9]*\) pages committed for boot.*/\1 \2/p' "$log" | tail -n 1)
    [ -n "$values" ] || return 1
    committed=${values% *}
    addressable=${values#* }
    [ "$committed" -gt 0 ] && [ "$committed" -lt "$addressable" ]
}

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
        secondary_cpus_online &&
        grep -q "HypeR: SMP online: $cpus/$cpus discovered CPUs" "$log" &&
        grep -q "HypeR: heap caches: $cpus CPUs, [0-9][0-9]* objects," "$log" &&
        grep -q 'HypeR: randomized kernel base 0x[0-9a-f][0-9a-f]*, KASLR offset 0x[0-9a-f][0-9a-f]*' "$log" &&
        grep -q 'HypeR: transition identity mappings retired' "$log" &&
        grep -q "HypeR: AArch64 host execution mode: $host_mode" "$log" &&
        grep -Eq 'HypeR: AArch64 execution protection: (XN|PXN/UXN), WXN=on' "$log" &&
        grep -q "HypeR: loaded VM 'alpine' from boot ramdisk: 128 MiB RAM, 1 vCPU(s)" "$log" &&
        demand_paging_is_lazy &&
        grep -q 'HypeR: memory (guest prepared): [1-9][0-9]* MiB RAM, [1-9][0-9]* reserved pages, [1-9][0-9]* managed pages' "$log" &&
        grep -q 'HypeR: page owners: guest [1-9][0-9]* (peak [1-9][0-9]*), user [0-9][0-9]*, page tables [1-9][0-9]*, kernel [1-9][0-9]*, heap [1-9][0-9]*' "$log" &&
        grep -q 'HypeR: kernel initialization complete; starting Linux guest' "$log" &&
        grep -q 'HypeR: vCPU 0 running as scheduler thread [1-9][0-9]* on guarded stack 0x[0-9a-f][0-9a-f]*-0x[0-9a-f][0-9a-f]*' "$log" &&
        grep -q 'HypeR test: AArch64 guest-entry IRQ mask contract passed' "$log" &&
        grep -q 'HypeR test: AArch64 IRQ-tail Fair vCPU preemption passed' "$log" &&
        grep -q "HypeR: periodic timer IRQs active on $cpus CPUs" "$log" &&
        # Host records may preempt the byte-at-a-time guest UART in the middle
        # of a line. `/init` and its userspace markers prove the stronger Linux
        # milestone without assuming that the early banner is contiguous.
        grep -q 'arch_timer: cp15 timer running at .* (virt).' "$log" &&
        grep -q 'Run /init as init process' "$log" &&
        grep -q 'HypeR guest: /init reached' "$log" &&
        grep -q 'HypeR guest: Linux userspace is running' "$log" &&
        grep -q 'HypeR guest: repeated timer wakeups passed' "$log" &&
        grep -q '^RX_OK' "$log"
}

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
    -kernel "$image" <"$input" >"$log" 2>&1 &
pid=$!

attempt=0
while [ "$attempt" -lt "$attempt_limit" ]; do
    if grep -Eq '<0>\[[ 0-9]+\.[0-9]{6}\].*(PANIC|BUG)|HypeR crash monitor|allocator invariant failure' "$log"; then
        cat "$log" >&2
        echo "HypeR reported a fatal failure during the AArch64 integration test" >&2
        exit 1
    fi
    if [ "$input_sent" = false ] && grep -q 'HypeR guest: repeated timer wakeups passed' "$log"; then
        # BusyBox ash asks the terminal for the cursor position before reading
        # its first command. Answer that query, then exercise guest RX.
        printf '\033[1;1Recho RX_OK\n' >&3
        input_sent=true
    fi
    if runtime_contract_is_ready; then
        if ! kaslr_geometry_is_valid; then
            cat "$log" >&2
            echo "invalid AArch64 KASLR geometry" >&2
            exit 1
        fi
        echo "verified $cpus-CPU AArch64 $host_mode/$atomic_backend host and Linux guest on QEMU CPU $cpu"
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
