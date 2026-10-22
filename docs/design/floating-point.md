<!-- SPDX-FileCopyrightText: 2026 roolrz -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Floating-point context ownership

AArch64 Native threads and guest vCPUs, and RISC-V Native threads, restore FP
state on first use in a machine run. Their valid integer-only runs do not load
or save that state. RISC-V guests restore once at each machine-run entry
for the platform reason below. Once loaded, the complete register image
remains resident across direct Native syscalls,
handled guest exits, and interrupts which return without scheduling.

The privileged AArch64 kernel uses `aarch64-unknown-none-softfloat`; RISC-V
retains `riscv64imac-unknown-none-elf`. Native applications continue using
their normal hard-float ABIs. Kernel Rust code, compiler builtins, and ordinary
assembly may not use FP/SIMD registers. The
linked-image test checks this requirement outside three audited assembly leaves
which initialize, restore, or save and clear an owner's state. An unexpected
kernel FP access is a fatal synchronous exception, never a request to borrow a
user's registers.

## Run boundaries

Each initialized AArch64 machine context owns all 32 128-bit vector registers,
FPCR, and FPSR. RISC-V owns all 32 64-bit F/D registers and FCSR. New contexts
start at zero. There is no independent CPU-local owner pointer: the existing
generation-qualified Native or guest run publication
identifies the only lower world permitted to acquire the registers.

Under VHE, `CPTR_EL2.FPEN` records residency for the interrupted lower world.
Exception entry saves CPTR in its integer frame and disables FP before calling
Rust. A first-use trap validates the published owner, restores its complete
state, enables access in the return frame, and retries the instruction without
advancing the PC. Guest CPACR_EL1 remains guest-owned; HypeR's EL2 control does
not override the guest operating system's own FP access policy. Optional SVE
and SME state remains unsupported and trapped.

On RISC-V, the saved HS `sstatus.FS` records residency, independently of the
owned `vsstatus.FS` policy. Trap entry disables physical FS before privileged
Rust runs. Because an FS-off Native instruction raises an ordinary illegal
instruction exception, the first illegal trap restores its bank and retries
the same PC. A genuine Native illegal instruction faults on the second attempt.
This conservative retry can restore FP for an illegal integer instruction,
but valid integer-only runs never restore FP. No instruction fetch or decoder
is required.

RISC-V guests retain normal `hedeleg` and restore once at every machine-run
entry, then keep the bank across direct traps. Guest FS policy is unchanged.
Lazy guest restore requires routing an FS-off illegal trap from VS to HS.
On the tested QEMU 11.1.2 / OpenSBI 1.8.1 combination, OpenSBI correctly prepares
a return with `mstatus.MPV=0`, but the CSR write leaves MPV set; `mret` then
attempts to execute the HS vector in VS and faults. This matches the QEMU
[change making MPV writes read-only](https://patchew.org/QEMU/20260616100527.1939565-1-alistair.francis%40wdc.com/20260616100527.1939565-5-alistair.francis%40wdc.com/)
(commit `18645f19578955ec5ff2c40cd2c8753d6bc460c2`). The safe policy does not
change installed firmware, guess emulator versions, or depend on a test-only
boot configuration. A future guest first-use policy needs a qualified trap
route on the supported platform; current RISC-V guests are not fully lazy.
Guest stop and IRQ-tail actions pass through one final save-and-scrub boundary
before the assembly anchor unwinds, including deferred device exits.

A real Native unwind saves used state before closing run publication. A Native
syscall runs with FP disabled in the kernel while its original private frame
records whether the user bank is resident. Only an actual scheduler switch
saves and clears that bank and withdraws publication; nested kernel IRQ frames
cannot substitute for the original syscall frame. The resumed syscall may run
on another CPU, and its final user return requires another first-use restore.
A syscall which never switches retains the bank, including allocation, ready
waits, unknown calls, and COW copies. Guest wait, device, terminal, and
administrative-stop exits save and clear before publication closes. AArch64
guest IRQ postludes are also ownership boundaries: they can deactivate,
schedule, migrate, or terminate the vCPU without unwinding the original
exception frame. Before
invoking such a callback, entry saves the state and marks the suspended frame
as requiring another first-use restore. This remains necessary when the
postlude happens to reconcile interrupts without changing CPUs. RISC-V saves
and clears the bank before its typed IRQ-tail anchor unwind, then eagerly
restores it when the guest runner begins the next run.

Save and release clears the physical vector registers and FP controls before
returning. Startup clears firmware state on every CPU. Consequently no prior
owner's register contents remain on a CPU after an ownership boundary, even
while a later owner has not yet used FP. All state leaves execute with local
exceptions masked and keep FP disabled on return. No cross-Thread FP owner
pointer or CPU-local residency reference survives migration. The private
syscall frame remains owned by its suspended Thread.

## Scope and validation

This is run-scoped lazy restore, not cross-run CPU ownership caching. An FP-using
thread may trap again after an actual scheduling boundary. A call that can block
but completes without switching does not create such a boundary. Retaining
state across that boundary would require another lifetime and migration
protocol and is deliberately outside this design.

Removing unconditional register copies reduces entry/exit work for integer-only
runs. This is not a blanket performance guarantee: an FP-heavy workload pays a
first-use trap after each ownership boundary, and the soft-float kernel also
changes the generated code for operations such as memory copies. Workload
measurements are separate from the isolation tests.

x86-64 retains its existing context handling; this does not add unsupported
Native or guest execution paths. RVV, SVE and SME register banks remain outside
the admitted execution contract.

The QEMU self-tests exercise integer-only Native execution, initialized FP
state, preservation across direct syscalls and actual scheduling, competing
owners, preemption, guest timer waits, and guest affinity migration. AArch64
also checks a real blocked syscall through repeated CPU migration, complete
FP/TLS preservation, cancellation, and an untouched COW output mapping. A
self-test-only gate admits a syscall with the original user FP bank resident,
then forces nested host-IRQ suspension while a competing FP owner runs. A
separate SMP gate makes a remote mapping update overlap masked syscall detach
and verifies that cross-call servicing completes the update before user return.
RISC-V probes also
verify genuine VS illegal-instruction delegation and the guest FS=Off policy.
The normal Linux guest acceptance also exercises Native/guest interleaving.
These establish architectural behavior
in the emulator; physical hardware validation remains necessary for
implementation-specific trap and speculative-execution behavior.
