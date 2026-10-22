<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Crash console

The `crash-console` Cargo feature adds an interactive, allocation-free monitor
to the fatal crash path. The default AArch64 feature set enables it; other
architectures leave it disabled. When disabled,
the monitor module, command strings, emergency UART input path, and stage-1
mapping inspection support are not compiled into the kernel image.

## Configuration

Include `crash-console` in the complete `KERNEL_FEATURES` selection, for example
`make image KERNEL_FEATURES='rtc runtime-symbols crash-console'`. No separate
defconfig or build target is required. Interactive operation also requires an `earlycon=` argument matching
an accessible UART in the platform DTB.

The crash owner masks local interrupts, switches to its emergency stack,
captures its context, broadcasts the crash-stop interrupt, and waits boundedly
for the other CPUs to publish their contexts. The owner prints the normal panic
diagnostics and then polls the permanently mapped emergency UART. Remote CPUs
remain stopped. The monitor does not use the scheduler, heap allocation,
sleeping locks, interrupts, or the normal serialized console path.

If no emergency console is available, fatal handling reports that fact and
halts instead of entering an unusable prompt.

## Commands

- `help`: show the command list.
- `status`: report the crash owner, remote stop result, and snapshot state.
- `cpus`: list every available captured CPU context.
- `regs [cpu]`: print the selected CPU's captured registers.
- `bt [cpu]`: print the selected CPU's bounded frame-pointer call trace.
- `mappings`: show the stage-1 root and configured kernel, linear RAM, no-map,
  and MMIO regions.
- `map <va>`: walk the live architecture stage-1 tables and report the leaf,
  physical address, mapping size, permissions, and memory type.
- `x <va> [bytes]`: dump 1 through 256 bytes from a readable mapping.
- `selftest`: validate the emergency console, immutable memory snapshot, owner
  context, remote CPU stop, and owner-PC mapping.
- `halt`: leave the monitor and enter the permanent architecture halt loop.

The monitor intentionally has no resume command. Kernel invariants cannot be
trusted after a fatal exception, and continuing would turn a diagnostic path
into an uncontrolled recovery mechanism.

## Early-crash debugger state

The full fatal path is enabled only after memory and exception stacks,
kallsyms, the scheduler, runtime exception vectors, and crash interrupt
resources have each published successful initialization. A panic before that
point does not access the console, switch stacks, unwind, or inspect another
subsystem. It stores the reason and CPU context in static storage, masks local
interrupts, and halts at a stable debugger inspection point.

The ELF exports stable symbols for this state:

- `hyper_crash_ready`: nonzero only when full fatal diagnostics are permitted.
- `hyper_early_crash_stopped`: nonzero when the guarded early-crash path halted.
- `hyper_crash_cpu_contexts`: per-CPU captured architectural contexts.
- `hyper_crash_payloads`: per-CPU panic reasons and owning contexts.

These symbols remain available to GDB independently of the interactive crash
console configuration.

## Memory-read policy

`x` validates the complete request before dereferencing it. Every byte must be
covered by a readable live stage-1 leaf whose translated physical range lies
inside DTB-described RAM and does not overlap a `no-map` reservation. Device
memory and unclassified physical ranges are rejected to avoid MMIO side
effects. A corrupted page table or failing physical memory can still cause a
recursive exception; the existing recursive-crash policy then halts without
attempting nested diagnostics.
