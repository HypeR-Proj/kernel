<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Kernel stack budgets

The default configuration gives ordinary scheduler threads, including Native
runners and vCPUs, 32 KiB guarded kernel stacks. IRQ and emergency stacks default
to separate 32 KiB mappings. These capacities are selected by
`CONFIG_KERNEL_STACK_SIZE_KB`, `CONFIG_IRQ_STACK_SIZE_KB` and
`CONFIG_EMERGENCY_STACK_SIZE_KB`; the audit ceilings below remain independent
of the configured capacities.
Early boot retains 256 KiB for allocation-free discovery and scheduler setup;
that capacity is not available to a normal syscall. Do not increase a runtime
stack or reduce an ABI batch merely to hide a large temporary.

Stack sizes must be powers of two: 16–256 KiB for Thread stacks and 16–128 KiB
for IRQ/emergency stacks. The configuration tool rejects unsupported sizes
before writing the output file; Cargo enforces the same constraints.

Stack count has no separate configuration limit. Stack-slot metadata and physical
pages grow on demand, while the architecture bounds the virtual stack arena.
Bootstrap owns a separate boot stack; each configured CPU has an IRQ and emergency
stack. The scheduler's address-stable slot segments also grow on demand. Retirement
uses links in those existing slots and never allocates during Thread exit. Tests
exercise more than 1024 simultaneous Threads and subsequent slot reuse; there is
no thread-capacity-sized temporary on the boot stack.
See [kernel configuration](https://github.com/HypeR-Proj/HypeR/blob/main/docs/usage/deployment/configuration.md#kernel-architecture-and-resource-configuration).

## Reproducing the checks

Run `make test-stack ARCH=aarch64`. This builds a dedicated initramfs in
`target/stack-audit/aarch64`, substituting the std workload for its `/bin/ps`
so the existing shell policy supplies thread inspection authority. The normal
application output and production initramfs are preserved. The workload checks
thread scan pagination, snapshot isolation, boundary/eager/COW mappings, and
dynamic child startup/exit, then stops the Linux guest to observe vCPU retirement.
The console attachment also exercises interactive guest input. For diagnosis,
`verify-stack.py --stage inspect|memory|process` isolates a workload stage so
its terminated thread can be measured independently.

The target explicitly enables both `kernel-test` and `kernel-stack-audit`.
Acceptance uses observed stack watermarks, canaries, and reserve limits for
complete workloads. The optimized image audit also bounds selected construction,
mapping and scheduling frames to catch large temporary regressions. In particular,
user-thread creation releases architectural context scratch before entering
scheduler admission. These frame checks supplement runtime acceptance; they do
not bound the complete call graph or every preemption point. No unstable compiler
flags or `.stack_sizes` metadata are required.

IRQ-tail load balancing projects only the thread fields needed by each migration
phase. Candidate-selection snapshots retire before queue mutation while the same
source CPU lock remains held. These boundaries reduce the common scheduling cost
below interrupted syscalls without changing placement or migration semantics.

Capability sends separate input admission from receiver matching and publication.
The image check bounds both the input frames and their combined size, so rollback
owners for the later delivery phase cannot occupy stack below sender input
admission's usercopy/COW and allocator calls. No message capacity or transfer
semantics change at this boundary.

Path resolution separates state construction, relative-ancestry reconstruction,
component lookup and canonical-result assembly. Traversal state lives in one
bounded allocation charged to the requesting domain and released with the request.
Preparation temporaries retire before backend lookup RPC; image checks bound each
phase and its live sum with the traversal coordinator.
This preserves space for IRQ-tail scheduling on an interrupted VFS syscall.
RAM and remote filesystem construction also finish before mount-point traversal,
so backend initialization scratch does not survive below namespace lookup RPC.

Process builder creation retires capability-copy and buffer-admission scratch
before reading the restricted loader snapshot, then constructs the builder in
a separate phase. File mappings likewise populate their source before constructing
the mapping transaction. These phase boundaries keep later construction temporaries
off filesystem reads, block submission and IRQ-tail scheduling. The image audit
bounds the coordinators and their separate preparation/construction frames.
Bulk handle reservations finish table growth before charge-record construction;
table backing is allocated only after its resource-admission temporaries retire.
Both phases retain the original lock order and abort reserved slots on failure.
Loader parsing retires its scratch before address-space construction and mapping;
the admitted segment table is borrowed without a second stack copy. Sealing builds
and publishes its final owner only after mapping succeeds. The image check bounds
the live seal/loader frames for parsing, construction, mapping and publication.

Block admission waits keep timed-wait registration scratch in a separate slow
path. Aligned filesystem reads do not retain the unaligned sector bounce buffer.
Filesystem RPC publication likewise excludes reply-wait scratch; single-object
waits finish charged signal-node construction before timer and scheduler entry.
Scatter destinations remain borrowed until the committed request completes;
splitting phases does not weaken that unconditional join or change batch sizes.

Block-backed filesystem transports allocate the 1040-byte extent plan, a
512-byte sector bounce buffer and sixteen batch descriptors once under the mount
sponsor, then reuse them under the volume mutex. All remain private to the kernel
and are released with the transport. Aligned and unaligned page-in paths carry
only borrowed descriptors below block completion waits. The optimized image check
bounds both read frames and their combined size; no per-read allocation is added
for this scratch.

`sh tests/ci/run.sh native-stack-16k` builds a production image with 16 KiB
kernel thread stacks and runs Native acceptance on one and four CPUs. This is
an independent CI job requiring init/root handoff, interactive shell commands,
static/dynamic application startup and confirmation of the actual stack size
in the boot log. It does not enable stack-audit instrumentation or change the
developer's `kernel/.config`.

The repository `board-storage` CI suite additionally builds with 16 KiB kernel
thread stacks, the minimum supported configuration. Storage-media and userspace
device acceptance require at least 4096 bytes of measured reserve, above the
general 2048-byte minimum. It exercises physical device claims, storage startup,
system-root handoff and guest startup on that
image. IRQ and emergency stacks retain their default capacities. This catches
overflow in long-lived services which do not contribute retired-thread samples.

If `board-storage-media` fails, CI also performs a bounded diagnostic boot using
QEMU GDB write watchpoints over the lowest 4 KiB of the first 256 stack slots.
It records new low-reserve observations and stops below the 4096-byte threshold.
The diagnostic replays the storage probe and a cross-filesystem copy on its disk
snapshot; `watch-stack.py --command` can select another bounded shell workload.
The existing artifact includes `stack-watch/report.json`, symbolized
`stack-watch/backtraces.log` and `stack-watch/serial.log`. This uses the matching
kernel ELF, accounts for KASLR and follows frame pointers only within the stopped
Thread's stack. It requires the AArch64 48-bit, 16 KiB CI configuration, uses a
snapshot of the already prepared board disk, and leaves that disk unchanged.
Watchpoints alter interrupt timing; a diagnostic boot that does not reproduce
the failure cannot turn the original acceptance result into a pass.

## What the evidence does and does not prove

Review complete hot/error/cleanup call chains; workload coverage does not
exercise every possible combination of calls or interrupt timing.

The optional audit samples only detached, terminated threads, after the switch
tail releases CPU ownership and before their stack is reclaimed. It checks
canaries and modified-byte watermarks. IRQ samples are restricted to the local
CPU, with migration and interrupts excluded during the scan. It never scans
another CPU's live stack. Logging reports the first sample and new maxima;
`samples` is the observation ordinal at that maximum, not a final sample count.

Watermarks can miss reserved but untouched stack space. Still-running services,
the reaper's own stack and IRQ stacks on unsampled CPUs are not covered. QEMU
workload coverage is evidence, not a proof against every stack overflow.
The harness requires retired kernel/user/vCPU and local IRQ observations, intact canaries
and at least 2048 bytes of measured reserve. It also rejects a measured maximum
above 24 KiB, independently of the allocated stack size. This leaves 8 KiB
between the watermark ceiling and the ordinary stack capacity (including the
canary). `STACK_MAXIMUM_USED` can override the workload ceiling.
`STACK_MINIMUM_REMAINING` can set a stricter reserve threshold. Guard pages catch out-of-range access, but exception
entry itself still requires space; recovery from arbitrary exhaustion is not
guaranteed.

## Performance comparisons

Use ordinary production images with `verify-stack.py --no-audit --repetitions 9`
and the same dedicated initramfs for both revisions. Audit scans and logging add
intentional overhead and must not be used to establish production performance.
The harness waits for the guest's idle prompt before measuring. Compare repeated
stage timings, accounting for QEMU and host scheduling noise; CI deliberately
does not enforce a wall-clock timing threshold.

For the Native syscall boundary, `tests/native/syscall-cost.c` provides a smaller
opt-in workload. It validates ABI query results, ready Event signal/wait pairs,
and eight-byte Channel roundtrips. Each stage warms up before nine measurements
of 4096 iterations. Output and capability construction are outside the timed
intervals. Build and package it with the installed SDK, from the repository root:

The SDK compiler driver uses `HYPER_CLANG` and `HYPER_LD` when set. Point them
at the local cross-capable Clang and LLD if those tools are unavailable under
their default names; this workload does not require another tool installation.

```sh
mkdir -p target/syscall-cost
HYPER_LINK_MODE=static target/sdk/aarch64/bin/hyper-clang \
  -std=c17 -O2 -Wall -Wextra -Werror tests/native/syscall-cost.c \
  -o target/syscall-cost/probe
make native-initramfs ARCH=aarch64 \
  NATIVE_IMAGE_PROFILE=development \
  NATIVE_INITRAMFS="$PWD/target/syscall-cost/initramfs.cpio" \
  NATIVE_PS_IMAGE="$PWD/target/syscall-cost/probe" \
  NATIVE_SERVICE_MANIFEST="$PWD/bootstrap/stage1/init/config/services-console-only.json" \
  NATIVE_VM_CONFIG=
QEMU_CPUS=4 python3 -B tests/qemu/measure-syscall-cost.py \
  --qemu qemu-system-aarch64 \
  --image kernel/target/aarch64-unknown-none-softfloat/kernel/hyper.img \
  --initramfs target/syscall-cost/initramfs.cpio \
  --log target/syscall-cost/candidate.log
```

Retain that exact probe and archive when comparing kernel revisions. Use
ordinary production kernels with identical configuration, QEMU CPU/memory/SMP
settings, and host load; record image hashes and all samples. The runner checks
completion and sample structure, and reports medians without a timing threshold.
QEMU timings describe the emulator workload, not physical hypervisor overhead.
Repeat the same workload on physical AArch64 with IRQ load, migration, mapping
replacement and ASID rollover before qualifying hardware behavior. Self-test
run/suspension and FP-copy counters, together with the optimized entry call
graph, explain which boundaries were removed; timings alone cannot prove that.

The runtime optimizations preserve eight-record inspector pages and serial
transfer limits. Caller-owned inspector output removes large aggregate return
copies. Snapshot and private-map initialization copy between pinned page
mappings directly, eliminating page-sized bounce buffers and a second copy.
Interactive serial writes use a 128-byte specialization; full 4096-byte writes
retain their complete buffer, one usercopy and one publication, preserving
failure and partial-write behavior without allocating per input event.

The userspace FAT worker directory parser fills a caller-owned entry and borrows its long-name
buffer. A mounted volume owns one persistent sector-cache allocation in place
of its former outer volume allocation, reducing mount return-value copies
without adding allocations or I/O. ELF segments retain their validated ELF
fields and derive page extents, reducing sorting scratch without replacing the
sorting algorithm. Process rollback extracts the address-space owner from its
existing unique allocation instead of moving the complete process state onto
the stack. Bootstrap and child startup release encoding scratch before later
thread and capability publication phases.

Physical device payloads are initialized directly in their final heap storage,
including their interrupt arrays. Object publication adopts that allocation
without copying the payload through constructor return values or syscall
frames. Allocation failure releases the physical claim and its resource charge;
the number and accounting of the final object allocations are unchanged.

Filesystem RPC returns only a scalar operation result through the blocking
transport. A separate decode frame validates the complete reply and fills the
volume's existing metadata scratch when requested. The reply's filename buffer
is not retained in each caller or across the scheduler wait path; the wire
format and validation remain unchanged.

`make test-stack ARCH=riscv64` runs the same workload and watermark checks.
Both architectures use the same runtime limits rather than per-function ceilings.
