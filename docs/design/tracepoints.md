<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Static kernel tracepoints

The optional `kernel-trace` production feature provides one-line instrumentation,
runtime category flags and bounded CPU-local recording. It is off by default and
independent of PMU sampling. It uses ordinary atomic flags, never instruction
patching, periodic polling or a tracing worker in interrupt dispatch.

`perf trace` controls recording through a delegated CpuInspector capability and
exports Perfetto-compatible Chrome Trace JSON or raw JSONL. Scheduler, Native
syscall, IPC, IRQ, page-fault, block-request and vCPU events use this mechanism.
The kernel exports fixed binary batches; names, correlation and viewer formats
remain userspace responsibilities. See the [capture guide](../../../docs/usage/diagnostics/tracing.md).

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
are internal kernel pointers. The wire encoder copies their bounded strings,
never the pointer; the collector interns them into export-local identities.

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
Control/read operations belong in ordinary thread context. The internal recorder
also supports kernel self-tests. Production userspace uses CpuInspector methods
260 (start, SET_ATTRIBUTES), 261 (stop, SET_ATTRIBUTES), and 262 (read,
INSPECT_DETAILS). Ordinary applications do not receive these rights; the formal
shell delegates its profiling authority only to `/bin/perf`.

Start accepts categories, a 256-bit online-CPU mask (zero means all), a 1–300000 ms
deadline and 64–4096 retained records per CPU. One ordinary callout closes admission
at expiry, then retries joining any short in-flight producer. No periodic tick or
remote IPI is required. Killing the collector does not leave recording enabled.
A privileged caller can stop/read another capture; generation tokens reject stale
control, but are not ownership capabilities.

The read selector chooses 160-byte capture info (CPU UINT64_MAX, zero generation
and cursor), a 72-byte best-effort live Thread name (CPU UINT64_MAX-1, cursor is
scheduler ThreadId), or a 2656-byte batch (eight records, ordinary CPU index).
Info includes build identity, selected CPUs, deadline, capacity and running state.
Each 328-byte record includes its site strings, source line and four scalar fields;
there is no deferred pointer/dictionary lookup. Strings are UTF-8, NUL-padded and
truncated to their fixed bounds. Retired threads may have no available name.
All output copying happens after recorder/control locks are released. These
observational IDs grant no scheduling or object authority.

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

Instrumented builds reserve 4096 records per configured CPU, with 56 bytes per
record on the supported 64-bit targets: 224 KiB per CPU plus a small aligned
control header. The buffers are static, not on thread/IRQ stacks. They stay
reserved while the instrumented image runs; builds without the feature have no
recorder buffers. The capture capacity selects a smaller overwrite window; it
does not release the compiled reservation. Capacity changes only at start, never
while producers use a capture. Each CPU header is cache-line aligned.

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
sequence. Use explicit correlation identities for causal relationships. The exporter
preserves loss information rather than inventing durations across gaps. Live
thread-name lookups are best effort and stay outside hot-path events.

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
for any enabled instrumentation. `make test-trace ARCH=aarch64` additionally
checks the installed SDK, authority, automatic expiry, CPU filtering, generation
replacement, stopped-buffer stability, loss reporting and real exported events.
