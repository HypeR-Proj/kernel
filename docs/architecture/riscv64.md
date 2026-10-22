<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# RISC-V 64-bit port

## Supported profile

The initial port deliberately targets one useful virtualization profile rather
than every historical RISC-V board:

- RV64GC host execution with the H extension
- Sv39 HS address translation and Sv39x4 guest-stage translation
- SSTC supervisor timer compare for guest virtual timers
- Zicbom cache-block management with a DT-described CBO block size
- SBI base, TIME, IPI, RFENCE, HSM, and SRST firmware services
- QEMU `virt`, OpenSBI, PLIC, ACLINT timers, and NS16550 early console
- Goldfish RTC for the UTC clock exposed through Native APIs and Rust std
- four host harts with standalone kernel mechanism tests

HypeR validates every enabled hart for the I/M/A/F/D/C, Zicsr and Zifencei
baseline, H, SSTC and Zicbom, and requires a consistent `riscv,cbom-block-size`.
Missing mandatory facilities are a
boot-time platform error; silently selecting a weaker execution model would
make guest behavior depend on accidental QEMU defaults.

The Rust target remains `riscv64imac-unknown-none-elf`. Ordinary Rust kernel
code therefore has a conservative soft-float baseline. Clang compiles the
small architecture assembly set with RV64GC+H but `-mabi=lp64`, keeping object
ABIs compatible while isolating H, F, D, and Zicbom instructions. Guest
floating-point state is initialized deterministically; the kernel restores it
once per guest run and saves and clears it at real ownership boundaries. The final
ELF audit rejects FP/vector instructions outside three state-transfer leaves.

Native applications use RV64GC LP64D through the installed RISC-V SDK, with
static or dynamic linking and the HypeR Rust std port. A dedicated U-mode trap
entry saves integer registers and disables physical FP access before Rust.
The run owns all 32 floating-point registers and FCSR; direct trap returns
retain the bank, while scheduling and deferred exits save and clear it.
[FP ownership](../design/floating-point.md) documents first-use illegal-instruction
retry for Native threads, per-run eager guest restore, separate HS/VS FS
controls, and migration invariants.
The runtime owns `tp`; kernel Rust executes with the restored host `gp`/`tp`.
Native calls use `ecall`, `a7` for the receiver handle, `a6` for its operation,
`a0`–`a5` for business arguments,
and `a0`–`a2` for status and results.

Native roots use the lower Sv39 canonical half for user mappings and share
supervisor-only kernel mappings in the upper half. ASID capacity is probed on
every admitted hart; implementations without hardware ASIDs retain software
identities and flush untagged translations. Tagged implementations lease ASIDs
from the epoch allocator and flush locally before entering a new epoch. Root replacement and
retirement use acknowledged CPU-local requests through the common residency
owner. Each consuming hart synchronizes its instruction stream before entry.

The Sv39 user range is 256 GiB. Current kernel layout policy limits
application-controlled VMARs to `[0, 0x0000_0020_0000_0000)` (128 GiB), reserving
`[0x0000_0020_0000_0000, 0x0000_0040_0000_0000)` for kernel-managed user mappings
such as a future vDSO. This reservation is separate from HS kernel mappings.
The boundary is provisional and may expand application space later; equal
halves are not an ABI requirement. Process ROOT_VMAR grants extend from one page to this application limit. See the [Native address-space contract](../design/syscall-abi.md#native-64-bit-application-address-space-contract).

## Architecture boundaries

Architecture code owns entry state, CSRs, traps, SBI instruction bridges,
Sv39 page tables, Sv39x4 guest translation, TLB fences, timer compare state,
and vCPU register layout. The shared HAL exposes semantic memory, interrupt,
timer, CPU-power, barrier, and cache contracts only. PLIC, NS16550, and SBI
policy live in reusable driver or platform layers; QEMU-specific discovery
selects them without exposing CSR details to the kernel.

The per-VM interrupt object has separate AArch64 and RISC-V implementations.
RISC-V does not instantiate the GIC/vGIC model merely to satisfy a shared type.
HVIP supplies the initial virtual local-interrupt mechanism; a future AIA
backend can add virtual IMSIC state behind that RISC-V implementation.

## Native reference guest platform

The immutable `Riscv64Reference` ABI profile uses RAM at `0x80000000`, an
NS16550A UART at `0x10000000` (4 KiB window, IRQ 10, 3.6864 MHz clock), and a
PLIC at `0x0c000000` (4 MiB window, sources 1–31, supervisor context 1).
The ABI schema owns these constants for kernel devices and userspace boot data.
The UART implements receive FIFOs and a reserved one-shot timeout; it has no
periodic polling worker. Teardown cuts registry visibility, closes device
producers and joins the exact timer callback before reclaiming its storage.

Guest traps capture VS state and current VS/VU privilege before entering Rust.
Returning exits carry a linear stopped proof which must detach the matching
local hardware owner. VMID width is probed on every admitted hart using a
permanent empty 16 KiB root. Implementations with fewer than eight VMID bits
use a logical eight-bit namespace and hardware tag zero, with full guest fences
on every entry and maintenance operation. Wider implementations use leased
hardware tags with epoch rollover. Acknowledged retirement precedes page reuse;
recycling an unpinned hardware tag requires a local epoch flush before use.

The Native creation-lease platform query reports the immutable counter frequency
and guaranteed guest ISA subset. Userspace uses these facts when constructing
the Linux device tree; it does not copy host ISA extensions or assume QEMU's
clock rate. The guest contract excludes nested virtualization.

`hyper-vm-image` validates the Linux Image header and the complete memory
footprint, including BSS, before `vm-runtime` copies payloads into a RAM VMO.
The SDK builds the CPU, interrupt-controller, UART and boot-data FDT nodes.
Kernel boot policy does not parse Linux images or create a default VM.

## Runtime validation

CI runs standalone four-hart kernel self-tests and separate one/four-hart
Native application acceptance. Native acceptance boots init, console/session
services and shell, and exercises static/dynamic std, threads, filesystem tools
and console input. The same init manifest and VM configuration used on AArch64
launch `vm-manager` and `vm-runtime` with a RISC-V Linux FIT payload. Native init
receives VM creation authority through the capability bootstrap; applications
cannot mint it. The VMM tests cover Linux userspace startup, paced bidirectional
console input, named VM isolation and repeated runtime-loss reclamation.

A separate `make test-vm-smoke ARCH=riscv64` fixture runs as `/init` and uses only
Native VM handles to construct small guest programs. It covers administrative
stop, timer and serial WFI wakeups, guest register preservation, privilege
isolation, owner-process loss and repeated stage-2 retirement on one/four harts.

## Current limitations

- QEMU `virt` is the only supported RISC-V board.
- PLIC supervisor context numbering follows the QEMU/legacy SiFive ordering;
  parsing `interrupts-extended` is required before supporting arbitrary PLIC
  topologies.
- SSTC is mandatory; the software-injected fallback is not a supported profile.
- The Native reference VM platform supports one guest vCPU. Guest SBI HSM,
  RFENCE and reset are not implemented; these are distinct from the SBI
  firmware services used by the host.
- Guest WFI traps to HS and blocks the scheduler execution until an enabled
  pending interrupt or a one-shot host deadline can wake it. Global guest
  interrupt masking does not suppress WFI wake conditions.
- Shared kernel stage-1 and active guest stage-2 invalidation use SBI RFENCE
  for other online harts. Native roots use the acknowledged residency protocol
  described above; a local fence alone is never treated as a shootdown.
- Cache publication and invalidation use Zicbom CBOs bracketed by full
  memory-and-I/O fences. Firmware must permit HS-mode CBO execution through the
  corresponding environment configuration.
- The host device support includes PLIC, NS16550 and Goldfish RTC; a platform-specific cache
  maintenance backend for hardware without Zicbom is not implemented.
