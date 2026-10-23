<!-- SPDX-FileCopyrightText: 2026 roolrz -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Kernel timers and callouts

The kernel time service multiplexes per-CPU deadlines onto the architecture's
local timer comparator, which is disabled when no consumer has a deadline. There is no
periodic scheduler tick. Scheduling decisions, ordinary timers, and exceptional
guest timer recovery share the earliest required deadline.

## Execution contexts

`Callout<T>` owns a typed callback context and is bound to one CPU. The default
constructor selects the creating CPU; explicit placement selects an admitted
CPU. Expiry transfers the preallocated delivery to that CPU's ready queue. A
pinned `ktimer/CPU` kernel Thread invokes the callback with interrupts and
preemption enabled, outside the timer and ready-queue locks. It does not inherit
the process or address space of the caller which armed it.
Timer workers use critical FIFO priority 32; current production callbacks only
publish reaper work or bounded PMU stop control. General background processing
must not be moved into these callbacks merely to obtain that priority.
The CPU suffix identifies its permanent owner. Timer workers reject affinity
changes and explicit migration, as well as automatic load balancing.

Callbacks must remain short and must not block. Work which waits for I/O or
other tasks belongs to a separate worker: blocking a timer worker would delay
every other callback assigned to it. Bounded batches provide scheduler safe
points between groups of callbacks; they cannot bound an individual callback's
execution time. A deadline is the earliest eligible execution time, not a
promise that the worker will obtain a CPU at exactly that instant.

Load balancing runs at scheduling events, without a periodic Callout or another
permanent Thread. It does not run inside the timer interrupt handler. See the
[scheduling boundary](../architecture/README.md#scheduling-boundary) for its placement
and migration rules.

`IrqCallout` notifications and `schedule_irq_*` callbacks execute outside the
deadline queue lock but inside the timer interrupt handler. They must not
sleep or allocate and must have
bounded execution time. Scheduler wait resolution retains this execution
model. IRQ and worker callback interfaces are deliberately distinct.

The scheduler uses a permanent per-CPU deadline slot rather than allocating a
timer node on each context switch. A single-writer atomic mailbox publishes
the decision while holding that CPU's scheduler lock; IRQ-tail or the final
resume boundary consumes it after releasing scheduler locks. The generation
includes cancellation and prevents old expiry delivery or delayed publication
from reviving a replaced decision. This avoids the reverse of the existing
timer-source-lock to scheduler-lock order used by Callout wait registration.
Expiry merely requests a safe scheduling point; it does not switch under the
deadline or interrupt registry lock.
Local publication also sends the coalesced timer prompt: an ordinary kernel
Thread may continue executing without reaching a resume boundary promptly.
An earlier explicit mailbox drain can make that one doorbell redundant, but
unchanged scheduling decisions publish nothing and create no periodic traffic.

## Ownership and cancellation

Construction allocates stable callback storage and a reusable timer node.
Arming, resetting, cancellation, and expiry do not allocate. A Callout can be
shared through a counted owner; its callback borrows the typed context and a
control reference which permits rearming without extending that borrow.

The source CPU's timer lock serializes deadline membership, ready membership,
callback claims, and close admission. A single Callout never invokes two
callbacks concurrently. Resetting it while its callback is executing records
the next deadline; completion publishes the replacement after the current
invocation has returned. Repeated resets replace the pending next invocation.

| Operation | Guarantee |
| --- | --- |
| `arm_at` / `arm_after` | Arm an idle Callout, or one successor to a running callback, at an absolute counter deadline or after a duration in nanoseconds. Reject an existing pending arm. |
| `reset` | Replace the pending deadline, or schedule the next invocation when the callback is already running. |
| `cancel` | Remove pending work without waiting; report idle, cancelled, or still running. |
| `drain` | Cancel pending work and join the selected running invocation; callers must coordinate subsequent rearming. |
| `shutdown_sync` | Permanently close arm admission, cancel pending work, and wait for callback completion. |

Synchronous operations require sleepable ordinary kernel-thread context and
must not hold locks required by the callback. They are rejected from every
timer worker, preventing both self-wait and cycles between workers. Shutdown
prevents the callback and other CPUs from restarting the timer during teardown.
Dropping a live Callout is an invariant failure; destruction never silently
blocks or frees a context which a callback may still access.

## Queue and clock representation

Each CPU owns a red-black tree whose stable keys are timer identities. Every
subtree caches its earliest deadline and enqueue order. Identity lookup avoids
linear searches during cancellation; the cached minimum provides constant-time
access to the next hardware deadline. Insertion, cancellation, resetting, and
expiry removal take logarithmic time. Equal deadlines preserve enqueue order
within a CPU queue; there is no cross-CPU callback ordering guarantee.

The tree owns its preallocated nodes through Rust ownership links. Rotations
and deletion neither allocate nor exchange one timer's callback payload with
another's. Allocation precedes queue publication, and detached storage is
released outside the queue lock.

Wrapping hardware-counter deadlines are converted into an extended monotonic
coordinate using a current counter sample taken under the queue lock. Tree
ordering therefore remains transitive even when an overdue timer and a distant
future timer coexist. External deadlines retain the counter's half-range
interpretation. Queue clock observations must advance within that unambiguous
range; disabling an idle comparator does not extend that counter interpretation
bound. Periodic IRQ
timers retain their original phase and report skipped periods instead of
replaying one callback for every missed tick.

`TimerEvent.observed_at` records the IRQ's expiry observation, not the later
worker dispatch time. Queue statistics count low-level expiry deliveries;
cancelling a ready Callout can suppress its callback after that count increased.
Ordinary Callouts are one-shot with explicit self-rearming; automatic periodic
delivery and overrun reporting belong to the IRQ timer interface.

The AArch64 guest timer has a separate fixed recovery slot. An observed timer
EOI while its physical level remains asserted starts a one-shot retry. Retries
begin after 50 microseconds and back off to at most one per millisecond until
the level deasserts. Unrelated maintenance, inactive guests, and ordinary masked
sources do not start polling. Hardware detach invalidates the owner's retry
generation, preventing a late callback from changing another vCPU's timer.
These exceptional retries and real guest/device interrupts remain possible on
an otherwise tickless CPU; removing the scheduling tick does not remove IRQs.

## Cross-CPU delivery

The timer remains attached to its source queue even if an arming or cancelling
Thread migrates. Remote changes to the earliest deadline publish durable
reprogramming state and prompt the source CPU, which updates its own hardware
comparator. The interrupt is a prompt, not the owner of that state.

Worker notifications use the shared deferred-work protocol to close the
empty-queue-to-sleep race. IRQ-tail service wakes the worker after interrupt
dispatch has released its registry locks. Future CPU hotplug must stop timer
admission and drain or transfer both deadline and ready queues before retiring
a CPU and its worker.

## Validation

Host tests compare queue operations with a reference model and check tree
balance, identity, cached minima, equal deadlines, counter wrap, and periodic
overruns. Bare-metal tests exercise worker execution context, per-CPU delivery,
self-rearming, and concurrent reset/cancel/shutdown using real scheduler and
interrupt paths. Controlled delivery windows additionally verify cancellation
and replacement after expiry, context destruction before deferred wake
retirement, and shutdown while a callback is running. The host timer queue
benchmark is manual and has no timing threshold in CI.

With both `kernel-test` and `kernel-self-test` enabled, each CPU also completes
three independently rearmed one-shot comparator probes. The sequence ends
permanently after its third expiry, so it cannot conceal a missing scheduler
deadline by providing a continuous fallback interrupt. Ordinary builds contain
neither these probes nor their fixed deadline slot. Host tests cover mailbox
publication races, stale/cancelled generations, counter wrap, and guest retry
owner changes and bounded backoff.

On AArch64, real kernel workers additionally prove that a sole Budget worker
has no scheduler expiry, remote contention reinstalls its deadline, and two
Q=P workers rotate across replenishment without yielding. This test observes
only the scheduler slot, so legitimate guest and callout interrupts do not
invalidate the assertion. RISC-V and x86 retain cooperative host continuations
and do not claim this host-interrupt preemption guarantee.

Run the allocation-excluding queue benchmark from the repository root:

```sh
cargo run --release --manifest-path kernel/tests/host/Cargo.toml --example timer_queue
```

QEMU validates integration, including one-CPU and SMP operation. Physical
AArch64 validation should additionally exercise concurrent remote rearming and
shutdown under load, confirm completion after shutdown, and measure IRQ and
callback latency. Host tests and QEMU do not establish hardware timing bounds.
