<!-- SPDX-FileCopyrightText: 2026 roolrz -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Scheduling policy and tickless execution

Scheduling policy is separate from execution kind. Kernel, Native and vCPU
Threads retain the same lifecycle, affinity, wait and context-save machinery.
`task/policy.rs` defines validated parameters; `scheduler/algorithms` owns pure
accounting and ordering; the CPU scheduler owns Thread membership and dispatch.
Ready indices use intrusive AVL nodes; updates keep a bounded ancestor path
instead of stacking recursive frames. Each CPU queue owns one heap-allocated
mutation path, prepared before the queue is published and reused by all its
indices under the CPU lock. The path bound covers the entire 24-bit registry
slot namespace; it does not impose a configurable Thread-count limit.
Candidate selection retains only
queue-head identities, keeping transient kernel-stack use independent of the
number of runnable Threads.

Changing an algorithm does not require changing the Native or VM entry path.
This is an in-tree extension boundary, not a runtime plugin or unload interface.

## Algorithms and arbitration

| Algorithm | Ordering and execution | Default use |
| --- | --- | --- |
| FIFO | Lower numeric priority first; equal priority keeps its turn until yield/block | Short trusted kernel timer callbacks |
| Fair | Minimum weighted elapsed-runtime pass; RR is a Kconfig alternative | Native apps and background kernel work |
| Budget | Q nanoseconds per common period P; bounded continuous turn B; unused CPU can be borrowed | vCPUs |
| Idle | Selected only when no eligible runnable work exists | Pinned per-CPU idle Thread |

Fair weight is 1–1024, with default 16. Under equal arbitration eligibility,
larger weight yields a larger long-term CPU share. This is a proportional
priority, not a strict ordering or a latency promise. Background kernel work
uses weight 32. The RR alternative preserves turn ordering but does not provide
weighted stride shares. One image uses exactly one Fair backend, selected by
Kconfig; a runtime request for the other backend is rejected. This keeps every
Fair queue's ordering keys in the same units. A Thread's band resolves ties; it
does not replace Fair weight with an unbounded strict-priority App hierarchy.
Within an identical arbitration tier, FIFO precedes Fair, then Budget; their
unlike numeric keys (priority, weighted pass and turn number) are never compared
to each other.

A separate role controls CPU arbitration:

- `ordinary`: normal algorithm entitlement. Ordinary FIFO is strict and can
  starve all lower work; only trusted holders may assign it.
- `priority`: can preempt normal Budget and Fair work while the CPU's shared
  priority allowance S remains. Once exhausted, the algorithm continues under
  its fallback rank. Creating extra service Threads does not create extra S.
- `protected`: participates in the CPU's shared R/P execution floor. When the
  remaining window is just large enough to deliver the remaining floor, this
  role precedes normal Budget and priority work. Strict FIFO can still delay it.

The floor is aggregate opportunity for runnable protected work, not a minimum
for every Thread. A protected FIFO Thread can dominate its protected peers;
FIFO is reserved for trusted bounded tasks. A Thread which exhausted its own
Budget is not owed protected execution until replenishment.

The default common period is 20 ms, priority allowance is 4 ms and protected
floor is 2 ms. Ordinary vCPUs use Q=14 ms, P=20 ms and B=10 ms. Critical service
manifests choose Priority/Fair; the I/O VM can select a Priority/Budget profile.
Ordinary apps default to Protected/Fair. Service configuration may override
these defaults within the launch authority's limits.

`CONFIG_SCHED_PREFER_GUEST` prefers entitled Budget work to ordinary Fair work.
`CONFIG_SCHED_PREFER_NATIVE` admits Fair work through the bounded priority
allowance instead. It does not put an unlimited Fair class above all vCPUs.
Both choices retain the protected floor and budget borrowing rules.

## Runtime and budget invariants

Accounting uses monotonic elapsed nanoseconds at scheduling boundaries and
observations, not a count of periodic interrupts. Fair charges preserve
fractional weighted execution across short runs. Wake clamps stale sleeper
credit to the CPU's virtual floor; migration translates debt from the source
floor to the destination floor. A Thread cannot start again at pass zero merely
by changing class. A larger Fair quantum limits scheduling frequency; it does
not grant extra weighted entitlement.

Budget replenishment is lazy at common period boundaries. Q is an entitlement
when there is competition, not a hard cap on an otherwise idle CPU. An exhausted
Budget Thread can borrow only when no competing non-Budget work is runnable.
Borrowed execution incurs no future-period debt. B prevents a Q=P Thread from
monopolizing a CPU shared with other Budget Threads; period rollover does not
reset an uninterrupted B. A sole vCPU needs no burst interrupt.

Yield, WFI, ordinary wake, migration and parameter changes cannot refill Q.
Reducing Q, B or a Fair quantum clamps remaining allowance immediately; raising
a value does not retroactively mint already-consumed time. Budget history
survives switching away from Budget and back. All vCPU budgets currently use
one configured P, allowing exhausted queue buckets to refresh at common epochs.
This is not deadline scheduling or admission-controlled bandwidth reservation.
If runnable budgets exceed a CPU's capacity, Q does not promise that each Thread
receives that amount or a proportional Q/P share. Eligible Budget peers rotate
by turn order and B within their arbitration tier. Budgets are per Thread, not
an aggregate per-process or per-VM limit.

## Deadline programming and balancing

When a CPU has no competing runnable Thread, the scheduler has no timer deadline.
With competition it selects the earliest relevant quantum, budget exhaustion,
period replenishment or protected-floor deadline. Other real software timers
remain active. A wake prompts a scheduling decision and deadline publication;
it cannot rely on a later tick to discover the competitor.

CPU-lock holders publish a generation-tagged atomic mailbox. The timer owner
consumes it outside scheduler locks and programs one comparator alongside other
timer sources. Stale expirations cannot reschedule a newer decision. Timer
service never acquires a scheduler lock while a CPU-lock holder waits for a
timer lock. See [timer ownership](timers.md) for the mailbox and guest recovery
contracts. AArch64 guest early-EOI recovery arms a separate bounded-backoff
one-shot only for that exceptional condition, not for every running guest.

Balancing is event-driven with bounded scans and transfers. Native initial
placement and migration prefer comparable CPUs without runnable Budget work;
a vCPU parked in WFI does not count as load. Movable ready Fair/Budget Threads
may migrate, but running contexts never move before the source save completes.
Pinned per-CPU workers reject both automatic migration and explicit affinity
changes. A moved Thread must actually execute before another automatic move.

One placement event scans a captured registry extent in finite passes of at
most 64 slots and four moves. A coalesced IRQ prompt continues unfinished
passes; newer events request a subsequent sweep without resetting the cursor.
Entering Idle requests a sweep, but remaining idle does not poll or rescan.
If an optional coordinator try-lock meets a reader or writer, it atomically
subscribes to that blocking lane's release. The real unlock supplies the next
prompt. There is no periodic balancing timer, migration Thread or IPI retry
loop polling an occupied lock. Event publication and pass ownership share one
atomic state so a request racing pass completion cannot lose its continuation.

Load snapshots and placement results use one heap workspace allocated during
scheduler construction. The coordinator grants its exclusive lease; a nested
placement cannot borrow it again, and recoverable errors return the lease.
Queue mutation uses each CPU queue's separate scratch, so migration never
borrows another placement workspace or allocates memory. Balancing remains in
normal Thread continuations (including the qualified IRQ tail), with local
interrupts masked until the pass returns. IRQ dispatch only publishes a prompt.
The workspace does not replace the Thread stack: call frames and the bounded
completion notification result still require stack headroom.

Strict FIFO can delay dispatch and balancing. The policy does not guarantee
NUMA placement, gang scheduling, per-VM aggregate shares or IPC priority
inheritance. AArch64 provides qualified host and guest IRQ-tail preemption.
RISC-V provides guest IRQ-tail preemption but retains cooperative host
continuations; x86 host continuations are also cooperative. The policies do not
introduce a host IRQ-tail capability on those secondary architectures or promise
bounded host preemption.

## Capabilities and configuration

Thread, self-execution and vCPU scheduling operations use separate rights:
`SCHED_LOWER`, `SCHED_PRIORITY`, `SCHED_CLASS`, `SCHED_BUDGET`, and `INSPECT`.
Ordinary object `WRITE` is not a scheduling elevation right. Changes validate
both the live target parameters and inherited ceilings while holding scheduler
ownership. Reductions must be monotone; cross-class operations require explicit
authority. Creating another Thread cannot restore a parameter already lowered
by its creator. Creating a Process uses an App launch boundary unless the
launcher explicitly grants a service profile. Later children of an ordinary
Fair caller retain its lowered weight and band; a service or another algorithm
crosses into the default App policy. A demotion does not revoke previously
created builders or dormant Threads, nor an independently held stronger factory
capability. This is per-object authority, not a process-wide resource quota.

Use the existing kernel Kconfig, component manifest and VM deployment JSON;
there is no second scheduler configuration store. Kconfig selects:

- `SCHED_FAIR_STRIDE` or `SCHED_FAIR_RR` and `SCHED_FAIR_QUANTUM_MS`;
- `SCHED_PERIOD_MS`, `SCHED_PRIORITY_ALLOWANCE_MS`, `SCHED_PROTECTED_MIN_MS`;
- `SCHED_VCPU_BUDGET_MS`, `SCHED_VCPU_BURST_MS`;
- `SCHED_PREFER_GUEST` or `SCHED_PREFER_NATIVE`.

Configuration rejects Q or B greater than P and S+R greater than or equal to P.
The Native ABI exposes validated typed scheduling parameters and read-only
observations. Runtime CPU and Thread statistics use elapsed nanoseconds; the
old CPU frequency word is reserved as zero. Applications such as `top` use
runtime deltas and do not assume a tick rate.
