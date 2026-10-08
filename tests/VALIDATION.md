<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Kernel validation coverage

Validation follows the property being checked. Cargo resolves dependencies,
rustc checks privacy and ownership traits, host tests execute reusable
mechanisms, image tests inspect linked artifacts, and QEMU exercises the kernel
with real scheduler and architecture entry paths. A successful source-text
search does not establish a lifetime, control-flow or memory-ordering proof.

`sh tests/ci/run.sh quality` from the repository root runs the workspace and
HAL checks and the host suites. `make -C kernel check ARCH=...` also compiles
the `kernel-self-test` configuration, including the ownership assertions below.
The [repository CI policy](../../tests/ci/README.md) lists the runtime suites.

## Compiler and artifact checks

| Property | Enforcement |
| --- | --- |
| Kernel -> HAL/core, HAL -> core dependency direction | `ci/hal-boundary.py` traverses Cargo's resolved graph, including indirect edges. |
| Private selected architecture implementation | A public HAL import must compile; importing `hyper_hal::arch` must fail at that import with E0603. |
| IRQ registration ownership, failure returns | `kernel/type_contracts.rs` rejects Clone/Copy and checks consuming activate/unregister/discard signatures and owned failure payloads. |
| CPU-affine IRQ, Native-entry, preemption and wait tokens | The same file rejects Send/Sync and Clone/Copy on the actual types. It observes manual trait implementations and nested fields. |
| Stopped guest/Native return ownership, address-space update/retirement cuts | The same file rejects Clone/Copy; behavioral residency and lifecycle tests exercise distinct transitions. |
| Rust/assembly frame layout | Architecture-local `size_of!`/`offset_of!` const assertions check the values supplied to assembly. All three architectures compile in CI. |
| Bootstrap stack headroom | Architecture-local const assertions check the final stack; `image/verify-image.sh` checks initial stack ELF symbols. Minimum: 256 KiB. |
| Delivered entry and translation instructions | `image/verify-image.sh` and `image/check-process-stack.py` inspect the linked image/disassembly. |

Compilation does not execute a destructor or prove that an unsafe caller uses
a token correctly. In particular, absence of Clone prevents duplicating a
capability through that trait; it does not establish quiescence or DMA retirement.

## Behavior and integration

Paths below are relative to this directory unless prefixed with `../`. These
are the places to extend when the corresponding invariant changes. Coverage is
not a claim that every interleaving or failure path has been exhaustively tested.

| Subsystem / former source-check family | Executable coverage |
| --- | --- |
| Boot phases and secondary handoff | `kernel/startup_readiness.rs`, `host/src/cases/cache_publication.rs`, CPU topology tests, and AArch64/RISC-V SMP boot acceptance. |
| IRQ registration, transition commit and replicated-local lifecycle | GICv2/GICv3 host controller tests; timer/serial setup and dispatch during kernel and Native QEMU acceptance. Ambiguous hardware completion still needs review. |
| Cross-call publisher pinning | Type checks on preemption guards; SMP mapping and stage-2 revocation via `kernel/user_memory_access.rs` and `kernel/stage2_blocks.rs`. Publication through acknowledgement is a review obligation. |
| Reschedule publication | `host/src/cases/scheduler_requests.rs` executes the production pending-request state; `kernel/reschedule_ipi.rs` exercises remote notification. |
| Thread migration and context handoff | `kernel/thread_migration.rs`, `kernel/vcpu_migration.rs`, and `kernel/switch_handoff.rs`; Native one/four-CPU suites exercise the selected assembly paths. |
| Thread resource aliasing, CPU ownership, stable thread table and retirement | `kernel/scheduler_parallel.rs`, scheduler queue tests and `kernel/wait_arbitration.rs`; host `scheduler_residence.rs` covers the ownership model. The host model alone does not prove scheduler implementation behavior. |
| vCPU transitions and interrupt reconciliation | `kernel/guest_entry_irq.rs`, `kernel/vcpu_migration.rs`, host `vm_interrupt_reconcile.rs` and virtual GIC tests; guest SMP/console acceptance exercises delivery. |
| Guest unwind, VM retirement and RISC-V register state | VM registry kernel tests; Native runtime/power crash, guest SMP and VM smoke acceptance; architecture-local runtime register validation. |
| Guest and Native address-space residency | `host/src/cases/address_space_residency.rs`, `kernel/stage2_blocks.rs`, `kernel/user_memory_access.rs` and Native user-entry tests. |
| Native HAL boundary | HAL compile probes; host `aarch64_user_contract.rs`; Native EL0/syscall and userspace application QEMU suites. |
| x86 stage-1 shootdown | Image instruction checks and architecture builds. There is currently no x86 runtime CI; broadcast/pinning/acknowledgement ordering remains a review and future hardware-validation obligation. |
| Allocator invariants and local caches | Host `runtime_allocators.rs`, `allocator_local_cache.rs` and `slab_partial.rs` exercise real allocation, corruption-handler installation, ownership and pressure paths. Early fatal-policy installation and allocation-free crash entry require review. |
| Deferred logging and crash diagnostics | Host `kernel_log.rs` and `crash_text.rs` execute ring, drain, formatting and emergency-gate mechanisms; `kernel/log_flush_barrier.rs` and console/crash QEMU acceptance exercise integration. |
| CPU-local timer pinning | `kernel/reserved_timer.rs`, `kernel/thread_sleep.rs` and host `software_timers.rs` exercise timer ownership and cancellation. Comparator programming within the CPU pin remains a review obligation. |

## Concurrent execution

`tests/concurrency` is a host-only Loom suite. From the repository root, run:

```sh
cargo test --manifest-path kernel/tests/concurrency/Cargo.toml --release --locked
```

It is also part of `make -C kernel test` and the CI `quality` suite. Its local
build script enables `cfg(loom)` only for this test crate, which includes the
production `DeferredWork`, `PendingReschedule` and `RunAdmission` source files.
Only their atomic imports and non-const test constructors differ. Do not set
global `RUSTFLAGS=--cfg loom`: the rest of the kernel is not instrumented.
The test dependency does not enter production images.

The bounded scenarios cover the following boundaries:

| Mechanism | Concurrent scenarios |
| --- | --- |
| `DeferredWork` | One or two producers racing worker sleep; one elected prompt and rearming after a completed batch; competing IRQ wake ownership after deferral; payload visibility at IRQ claim, batch start and a retained batch. |
| `PendingReschedule` | Acquire observation and consuming requests; competing publishers electing one notifier across two reused epochs; a coalesced publication acquired by the current take or preserved as a new pending epoch. |
| `RunAdmission` | Close racing one or two admission attempts; competing closers preserving live claims until the last release; quiescence acquiring both runners' final writes after concurrent releases. |

Payloads are sampled before publisher joins so test synchronization cannot
supply the missing publication edge. Assertions made after a join use those
saved samples when checking visibility. Scenarios use finite operations instead
of polling loops; the suite does not set an exploration/preemption limit. This
explores the specified scenarios within Loom's memory model, not every kernel execution
or every hardware weak-memory behavior.

`kernel/wait_races.rs` runs 32 rounds with two resolver threads on separate
CPUs, racing exact notification against cancellation or timeout arbitration.
Half the rounds keep the registration Armed; the others require real queue
publication and race the switch tail/parked wait. It checks one winning outcome,
one notification callback only when notification wins, exactly one ready
publication for queued waits, an empty queue and rejection of retired tickets.
Worker completion is followed by real scheduler/reaper quiescence. This tests
the timeout resolution path directly; timer interrupt delivery remains covered
by the existing timed-wait tests.

The SMP case needs at least three CPUs and explicitly skips otherwise. AArch64
acceptance checks that marker and explicitly selects multithreaded TCG. These
are correctness checks with progress deadlines, not performance benchmarks.
Physical AArch64 stress remains necessary for hardware ordering and cache/TLB
behavior; neither this suite nor QEMU qualifies physical DMA retirement.

## What needs review

Retired source checks also searched for particular variable names, exact call
counts, literal register assignments, forbidden old type names and apparent
statement order. Those assertions and their text-mutation fixtures are removed,
including equivalent `include_str!` assertions in host tests. They could reject
an equivalent refactor or accept unreachable code and did not execute the
ownership or failure path they described.

Review changes to unsafe publication, lock ordering, IRQ mask windows,
cache/TLB/device barriers, fail-stop paths and lock-external destruction at the
owning implementation. Existing safety comments and design documentation retain
these obligations. QEMU does not prove physical cache, speculative access or
weak-memory correctness. Where a regression needs new automated coverage, test
the actual state transition, rejected API use, linked artifact or runtime
outcome. Do not replace the removed greps with a parser that merely asserts the
same AST spelling, or a second toy implementation of the mechanism.
