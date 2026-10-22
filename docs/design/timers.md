<!-- SPDX-FileCopyrightText: 2026 roolrz -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Kernel timers and callouts

The kernel time service multiplexes per-CPU deadlines onto the architecture's
local timer comparator. The scheduler tick is one consumer of this service;
ordinary deadlines do not wait for the next periodic tick.

## Execution contexts

`Callout<T>` owns a typed callback context and is bound to one CPU. The default
constructor selects the creating CPU; explicit placement selects an admitted
CPU. Expiry transfers the preallocated delivery to that CPU's ready queue. A
pinned `ktimer/CPU` kernel Thread invokes the callback with interrupts and
preemption enabled, outside the timer and ready-queue locks. It does not inherit
the process or address space of the caller which armed it.
The CPU suffix identifies its permanent owner. Timer workers reject affinity
changes and explicit migration, as well as automatic load balancing.

Callbacks must remain short and must not block. Work which waits for I/O or
other tasks belongs to a separate worker: blocking a timer worker would delay
every other callback assigned to it. Bounded batches provide scheduler safe
points between groups of callbacks; they cannot bound an individual callback's
execution time. A deadline is the earliest eligible execution time, not a
promise that the worker will obtain a CPU at exactly that instant.

The Fair scheduler's load-balancing Callout uses this existing worker context
for bounded periodic correction. It does not create another permanent Thread
or perform balancing from the timer interrupt handler. See the
[scheduling boundary](../architecture/README.md#scheduling-boundary) for its placement
and migration rules.

`IrqCallout` notifications and `schedule_irq_*` callbacks execute outside the
deadline queue lock but inside the timer interrupt handler. They must not
sleep or allocate and must have
bounded execution time. Scheduler ticks and scheduler wait resolution retain
this execution model. IRQ and worker callback interfaces are deliberately
distinct.

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
range; the periodic scheduler tick supplies regular observations. Periodic IRQ
timers retain their original phase and report skipped periods instead of
replaying one callback for every missed tick.

`TimerEvent.observed_at` records the IRQ's expiry observation, not the later
worker dispatch time. Queue statistics count low-level expiry deliveries;
cancelling a ready Callout can suppress its callback after that count increased.
Ordinary Callouts are one-shot with explicit self-rearming; automatic periodic
delivery and overrun reporting belong to the IRQ timer interface.

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

Run the allocation-excluding queue benchmark from the repository root:

```sh
cargo run --release --manifest-path kernel/tests/host/Cargo.toml --example timer_queue
```

QEMU validates integration, including one-CPU and SMP operation. Physical
AArch64 validation should additionally exercise concurrent remote rearming and
shutdown under load, confirm completion after shutdown, and measure IRQ and
callback latency. Host tests and QEMU do not establish hardware timing bounds.
