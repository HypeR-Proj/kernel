<!-- SPDX-FileCopyrightText: 2026 roolrz -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Memory diagnostics

`free` is the physical-memory summary. `memstat` is the developer view for
investigating growth after operations such as starting and stopping a VM. It
reports existing allocator, page-owner, buddy and file-cache counters. Optional
heap profiling adds occupancy and allocation/free traffic by pool and size.
Neither tool forces reclamation, drains CPU magazines, or changes cache policy.

## Capture and compare

Run from the formal shell (the tool is installed at `/bin/memstat`):

```sh
memstat
memstat --save /tmp/before.json
vmm start alpine
vmm stop alpine --wait
memstat --diff /tmp/before.json
```

Wait for `reply RELEASE_MEMORY: ok` when testing backend retirement, then allow
asynchronous service exit/reclamation to settle. Repeat the operation and capture
another sample if growth continues. Use `--classes` to include the optional heap
buckets, or `--json` for the complete machine-readable record. Saved snapshots
always include all available buckets, even without `--classes`. `--save` creates
a new file and refuses to overwrite an existing baseline.

Compare only within the **same boot and configuration**. `/tmp` is appropriate
for baselines. The tool rejects backwards time/counters, differing metric sets
and differing profiling configurations. Those checks are not a boot identity:
an old snapshot can still pass them after a reboot. Snapshot files are versioned,
limited to 128 KiB, and parsed as data only.

Use `free` alongside these records for its validity-aware reclaimability
estimates.

The report separates **gauges** (current or peak occupancy) from **counters**
(cumulative requests/events). A large allocation-counter increase with stable
live occupancy indicates churn, not retained memory. `heap.allocations` counts
requests, including failures; heap-class allocations count successes only. Positive occupancy growth
alone is not proof of a leak. First use of code, cache warm-up, delayed teardown,
and the observer itself can grow memory.

## Reading the views

| View | Evidence it provides |
| --- | --- |
| `physical.*` | The raw physical accounting behind `free`, without merging heap and page tables into the kernel summary |
| `owner.*` | Current/peak pages and allocation/failure/free counts for kernel pages, page tables, guest pages, user pages and file-cache payloads |
| `buddy.order_N.free_blocks` | Free blocks at each order; one block contains `2^N` pages, useful for distinguishing exhaustion from fragmentation |
| `heap.*` | Live caller objects and requested bytes, peak usage, allocation failures, CPU-magazine occupancy, hits/misses/refills/drains and pressure reclamation |
| `file_cache.*` | Payload pages, physical metadata backing, index capacity, clean/loading entries, live payload objects, admission and maintenance state |
| `heap_class.POOL.SIZE.*` | Optional per-size occupancy and traffic; pools are `kernel` and `file_cache`, sizes are 16 through 2048 bytes plus `large` |

These are **overlapping views**, not amounts to sum. For example:

- `physical.kernel_bytes` is direct kernel-owned page backing, while `free`'s
  displayed kernel category also combines non-reclaimable heap and page tables.
- File-cache metadata slab/large pages are charged to the dedicated cache heap
  pool and are already included in `physical.file_cache_bytes` (the physical
  slab/large-heap fields exclude that pool). Metadata budget
  pages are a budget measurement, not additional physical pages.
- A slab bucket's `capacity` is slots in its resident slab pages. `reserved`
  includes caller-owned slots, CPU-magazine slots and transfers in flight.
  `live` counts caller allocations; `cached` counts magazine entries.
  At quiescence, `reserved = live + cached`. Do not require this equality while
  other CPUs allocate. Size-class slack and slab headers are not requested bytes.
- Large buckets report page backing and live/requested bytes; slot capacity,
  reserved and cached are zero. Their class size is represented as `large`.

In the VM-retirement investigation, the useful sequence is: compare guest and
page-table owners; check whether heap live counts or merely cached/reserved slots
grew; then check file-cache payload, index capacity and metadata backing. A pool
and size bucket narrows the source but does not identify a function or prove that
any particular reference is leaked. Use `vmm list` and `handle` for existing
VM/object ownership observations.

## Optional heap profiling

The `heap-profiling` kernel feature is disabled by default. Enable **Detailed
heap accounting** (`CONFIG_FEATURE_HEAP_PROFILING`) under **Kernel → Optional
kernel functionality** in `make menuconfig`. An isolated build can also override
the selected production feature set:

```sh
make image ARCH=aarch64 \
  KERNEL_FEATURES=rtc,runtime-symbols,crash-console,heap-profiling
```

This enables 18 fixed buckets (two pools, eight slab classes and one large bucket).
On the supported 64-bit targets, counters add 1,008 bytes inside the allocator:
432 bytes of backing counters and 576 bytes of traffic atomics. There are no
per-object records, captured addresses, backtraces or profiling allocations.
Successful allocation/free paths update per-bucket atomics, and central slab/page
transitions update backing counters. This can contend under heavy allocation;
use profiling builds for diagnosis, not comparative performance measurements.
When disabled, those fields and updates are compiled out. General diagnostics
remain available, and `memstat` explicitly reports that class profiling is off.

Kernel diagnostics require `MemoryInspector` with `INSPECT`. The shell delegates
it only to the trusted `/bin/memstat` path, just as it does for `/bin/free`.
The API returns counters only, not kernel addresses or mutation authority.
`hyper_os::inspect::MemoryInspector` offers typed heap, buddy, owner, file-cache
and heap-class reads. The topic/record contract is defined by
[`sdk/abi/schema/native.rs`](https://github.com/HypeR-Proj/abi/blob/main/schema/native.rs).

## Sampling limits

Each request is bounded. Heap/buddy backing is copied under its existing lock;
CPU magazines are sampled individually, never while holding the central heap
lock. Atomic logical counters, cache state and separate topic reads can describe
different instants. The tool records a monotonic sampling interval, and collects
fixed-size records before allocating its report map. Launching the tool still
uses process memory, faults executable pages and may populate caches. Saving a
baseline also consumes filesystem memory after that sample; repeat with the
same workflow rather than treating tiny deltas as definitive evidence.

Heap-class allocation/free counters saturate at `u64::MAX`. No reset operation
is exposed. This patch does not add per-callsite stack tracing, per-object
allocation histories, automatic leak classification, or a global object-graph
walk.
