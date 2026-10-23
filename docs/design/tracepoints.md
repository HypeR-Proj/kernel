<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Static kernel tracepoints

The optional `kernel-trace` production feature provides one-line instrumentation,
runtime category flags and bounded CPU-local recording. It is off by default and
independent of PMU sampling. It uses ordinary atomic flags, never instruction
patching, periodic polling or a tracing worker in interrupt dispatch.

This patch provides kernel-internal control and read interfaces. It does not add
a userspace tracing capability, a `perf trace` command, Perfetto serialization or
general scheduler/IPC/I/O instrumentation. Those consumers can use the recording
mechanism without making the kernel depend on a viewer's file format.

## Add an event

At a committed state transition, add one statement:

```rust
crate::tracepoint!(Scheduler, "thread_ready", thread = id.get(), cpu = target.get() as u64);
```

The macro creates static metadata for category, event name, field names, source
file and line. There is no separate event-number list, registration function or
linker-section table to maintain. Different call sites can use the same name;
the static site identity and source location still distinguish them. Metadata
for sites that have emitted events is available through their records; there is
no enumeration of every compiled site in this version.

The macro belongs to `hyper-core` and is re-exported by the kernel binary.
Reusable mechanisms (including allocator code) use the same one-line call as
kernel policy. During startup, the kernel installs one immutable machine adapter
for CPU pinning and clock access. Only enabled events call it, passing a borrowed
payload closure without allocation. No portable module imports the kernel or HAL.

Each event carries zero to four named `u64` scalar values. Encode booleans/enums
explicitly, and use domain accessors for identifiers. Do not record raw addresses,
user memory, arbitrary strings or process-local handle encodings. Site references
are internal kernel pointers and must be replaced with export-local IDs by any
future serializer. No internal pointer belongs in an exported record.

Arguments must be cheap, side-effect-free and nonblocking. Use values already
available at the call site; do not query other objects or acquire subsystem locks
for tracing. Record a decision separately from its completion when those are
distinct transitions. The timestamp is captured before evaluating the fields,
after admission on the pinned CPU.

Keep the recorder, adapter entry and the low-level primitives used to enter it
uninstrumented. Recursion after the ring has been acquired is safely dropped;
recursion before reaching that guard is not a supported instrumentation point.

Without `kernel-trace`, the macro expands to an empty expression: it creates no
site, evaluates no fields and accesses no runtime flag. Instrumented builds test
the category flag before masking IRQs, reading the clock or evaluating fields.
Disabled builds need not type-check expressions used only inside tracepoints;
the feature-enabled compile/test gate is therefore required.

## Capture lifecycle

The machine adapter is `kernel::trace`; portable storage lives in `hyper::trace`.
Control/read operations belong in ordinary thread context. The control interface
is internal, not authority granted to applications.

```rust
let capture = trace::start(Filter::only(Category::Scheduler).with(Category::Ipc))?;
trace::set_filter(capture, Filter::NONE)?; // Pause new admission, retaining data.
trace::set_filter(capture, Filter::ALL)?;  // Resume within the same generation.
trace::stop(capture)?;                    // Retry BUSY before claiming completion.
let batch = trace::read(capture, cpu, cursor, &mut records)?;
```

Start rejects an empty filter and a capture not yet successfully stopped. It
allocates nothing and replaces the previous capture, using a new non-repeating
generation. Exhaustion is reported rather than wrapping capture identity. Rings
reset lazily on their first record of that generation; old contents are never
returned as new records. Repeated stop and non-destructive reads are supported.

Changing filters can leave an already admitted event in flight. Once stop has
acquired control it closes admission, then tries each CPU ring to join writers.
It never spins. `BUSY` requires retry; a new capture cannot start until stop has
succeeded. A delayed old ticket is rechecked under the ring lock and cannot write
into the replacement capture or mark its loss state. Stop freezes each CPU's loss
stamp after joining its ring; rejected attempts arriving beyond that capture's
cutoff cannot mutate the completed snapshot. No cross-CPU IPI is needed:
writers are short software operations, with no remote hardware state to disarm.

## Storage, concurrency and loss

Instrumented builds reserve 256 records per configured CPU, with 56 bytes per
record on the supported 64-bit targets: 14 KiB per CPU plus a small aligned
control header. The buffers are static, not on thread/IRQ stacks. They stay
reserved while the instrumented image runs; builds without the feature have no
recorder buffers. Each CPU header is cache-line aligned.

The machine adapter masks local interrupts through CPU selection, timestamping
and append, restoring the caller's exact prior mask. Rings use the existing
`SpinLock` exclusively through `try_with`; neither producer, control nor reader
waits for a lock. An interrupted/recursive producer or a competing reader can
cause loss, but never makes the producer spin on an interrupted owner. There is
no global writer counter or cross-CPU producer lock. Record append and batch copy
are protected by the same acquire/release lock, preventing torn records.

The maximum read batch is 32 records, regardless of the caller's output size.
Only the returned prefix is valid. Cursors are per CPU and capture. A zero-count
read during capture means no record is currently available, not end-of-stream.
After successful stop the captured contents are stable until the next start.

Loss is explicit:

- Sequence/cursor gaps count committed records overwritten by the bounded ring.
- `incomplete` means at least one event could not be recorded due to contention,
  recursion, timestamp unavailability or sequence exhaustion. It is a sticky
  per-CPU, per-capture flag, not an estimated event count. A single tagged CAS
  sets it without retries; competing setters for that generation converge on
  the same flag.

There is no guaranteed total order across CPUs beyond timestamps and per-CPU
sequence. Use explicit correlation identities for causal relationships. A future
exporter must preserve loss information rather than inventing durations across
gaps, and snapshot object names/lifetimes separately from hot-path events.

## Validation

Host tests exercise the production recorder for overwrite/cursors, independent
CPU writers, concurrent reads, paused filters, stale tickets, recursive calls,
clock failure and stop/restart racing an admitted writer. Both macro variants
are compiled, including an undefined argument that only the disabled expansion
can erase. Feature-enabled kernel self-tests record on every online CPU, preserve
the caller's IRQ mask and record from actual IRQ-context timer callbacks.

```sh
make -C kernel check ARCH=aarch64 KERNEL_FEATURES='default kernel-trace'
make -C kernel test-qemu ARCH=aarch64 KERNEL_FEATURES='default kernel-trace'
```

The self-test target also selects both `kernel-test` and `kernel-self-test`.
Production tracing does not enable tests. QEMU validates integration, not a
hardware tracing-overhead bound; real-board workload comparisons remain needed
when production event points and a collector are added.
