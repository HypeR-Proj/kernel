<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# SMMUv3 driver and DMA acceptance

The non-secure SMMUv3 driver implements host-owned stage-2 DMA translation.
The QEMU acceptance fixture uses real PCI EDU DMA endpoints behind
`virt,iommu=smmuv3` for both reads and writes.
Use QEMU 9.2 or newer and a `virt-9.2` or newer machine model. Earlier `virt`
models expose only stage 1; see the upstream
[machine compatibility rule](https://github.com/qemu/qemu/blob/v9.2.0/hw/arm/virt.c#L3101-L3107).
The register and descriptor contract follows
[Arm IHI 0070 G.b](https://documentation-service.arm.com/static/6813b2bbefb0f21122c144c2).

Default QEMU storage/network assignments use **virtio-PCI** and bind their
requester IDs to owned domains. Initial VM RAM and live shared-memory grants
retain DMA leases until completed invalidation. See [I/O isolation](io-isolation.md)
for the production ownership and retirement sequence. Dedicated **virtio-mmio**
compatibility tests remain; that transport bypasses the PCI SMMU. Pi 5 uses
different hardware and remains a trusted-I/O-VM deployment, including SDHCI.

## Ownership and admission

- `drivers/iommu/smmuv3` owns register programming, queues, stream entries,
  DMA translation tables and synchronization. It has no Native syscall policy.
- `kernel/device/iommu` supplies owned `PageBlock` allocations and selected-HAL
  barriers. Platform initialization activates a discovered SMMU before physical
  device publication. Translation providers (including unsupported providers
  with `#iommu-cells`) and generic ECAM apertures are host-reserved in the
  firmware catalogue, including overlapping MMIO aliases and host IRQs.
- Discovery currently admits one SMMUv3 controller. It requires coherent
  table/queue access in both firmware and IDR0, little-endian VMSAv8-64 tables,
  stage 2, 4 KiB granules, terminate faults and software-owned queues/tables.
  Unsupported or already-active hardware fails initialization without falling
  back to bypass. Firmware must quiesce upstream DMA before kernel entry.
- StreamIDs come from the PCI host's `iommu-map` and optional `iommu-map-mask`.
  The decoder rejects missing, ambiguous, malformed and overflowing routes.
  Equal routed StreamIDs cannot be assigned to separate domains.
- A linear stream table covers up to 16 StreamID bits; larger IDs abort rather
  than alias. The table consumes at most 4 MiB. Domains use sparse 4 KiB tables
  with 39-bit IOVAs and up to 48-bit physical addresses, bounded by IDR5.
  Unsupported address geometry is rejected; no address is silently truncated.
- Each domain has an SMMU-local VMID and a nonrepeating software generation.
  VMIDs can be recycled only after domain retirement. Stale domain identities
  cannot attach streams to a new owner.

The controller is serialized by a sleeping mutex and permanently retained by
the platform bus and its fault worker. Allocation and hardware completion waits
never run under an IRQ-masked queue lock or in the IRQ callback. Payload mappings
retain kernel-only BackendMemoryLease objects independently of source handles
and CPU mappings. Production PCI assignment installs the domain before enabling
bus mastering; live backend grants extend it and complete invalidation before
releasing their leases. The [authority and ownership contract](io-isolation.md)
describes that lifecycle. There is no second userspace handle namespace.

## Translation and retirement

Initialization sets GBPA to abort, installs zeroed stream entries, and enables
the command queue. Configuration/TLB invalidation and `CMD_SYNC` complete before
translation and event delivery are enabled. Unassigned streams remain invalid;
the driver never installs bypass STEs. ATS and PRI are not admitted. If ATS is
implemented, CR0.ATSCHK enables checking of Translated traffic and EATS remains
zero, avoiding the architecture's fast-mode bypass.

Payload mappings own retained buffers and express read-only, write-only or
read/write access. `map_buffer` admits a complete buffer (including physically
scattered pages), preallocates its intermediate tables and retains its owner
before leaf publication. `map_page` is the one-page convenience form. Control
tables/queues retain a separate exclusive allocation interface; payloads expose
no CPU pointer to the driver. Descriptor publication is ordered with full-system
barriers. Mapping replacement requires removal first. `unmap_buffer` clears all
leaves of the original mapping, detaches empty intermediate tables, invalidates
the domain's TLB entries and walk caches, and waits for `CMD_SYNC` before freeing
those tables or returning the buffer owner. Table-entry counts allow bottom-up
pruning without scanning every leaf or allocating retirement metadata. Command
failure retains both detached tables and payload owners. Long-lived domains
also compact spare metadata after successful unmap; an allocation failure leaves
the existing metadata intact without failing completed retirement.
This covers the affected transactions' architectural completion
semantics, not just command consumption. ATS translation caches cannot retain
old translations because ATS is disabled.

Root and L2 owners carry a 512-slot software child index (2 KiB each); leaf
owners have no child array. Walks use these indices and validate them against
the hardware descriptors, without searching the domain's table-owner vector.
Parent indices support linear bottom-up pruning. After `CMD_SYNC`, a linear
compaction pass repairs parent and child indices before dropping detached owners;
the relocation scratch is embedded in each owner, so repair cannot fail to
allocate. A page lookup has fixed depth; unmapping P pages with T table owners
and M buffer mappings costs O(P + T + M), including the sorted mapping-vector
removal. Repeated small buffer removals still each scan the live table owners.

Stream reassignment requires detach followed by attach. Detach invalidates the
STE and completes configuration synchronization before invalidating domain
translations. The device must be quiescent before assignment, and pending fault
records must be consumed and handled before attach; otherwise it returns
`FaultsPending`. Event records carry a SID but no assignment generation, so an
old record must not be attributed to a replacement binding. The check covers the
whole event queue, including faults from other streams; the fault worker drains
them before the caller retries. Attach fills the invalid STE, invalidates and synchronizes it,
then publishes its valid bit and repeats configuration synchronization. This
prevents speculative reads of old words from being combined with a new valid
bit. The driver follows IHI 0070 §3.21.3.1 even where QEMU does not model such
speculation.
Domain destruction requires no bindings and no mappings, and completes a final
invalidation before freeing its page tables or reusing its VMID.

The caller must additionally quiesce a device before reusing an IOVA for another
owner: an IOMMU cannot distinguish a newly issued stale request from legitimate
work using that new address. Disabling PCI bus mastering alone is not a DMA
completion proof. A failing command, global error or event overflow stops new
driver operations. Published controllers quarantine their queues, tables and
remaining mapped pages on Drop; they do not free memory still reachable by
hardware, reset to bypass, or automatically retry a partial revocation.

Command completion remains polled with a bounded timeout. Faults are handled
by wired interrupts and a kernel worker as described below. Noncoherent
maintenance, two-level stream tables, ATS/PRI/stall admission and controller
reset/recovery are not advertised.

## Runtime faults and failure handling

The driver accepts separate `eventq`/`gerror` GIC SPI routes or a single
`combined` route. Firmware names, descriptor counts, trigger encodings and
aliases are validated before hardware admission. IRQ registration failure
unwinds previously installed handlers. MSI target addresses are explicitly
cleared while IRQ delivery is disabled, selecting wired delivery even if
firmware left unknown MSI targets. PRI and interrupting CMD_SYNC are unused.

The IRQ callback masks its interrupt-controller route and publishes durable,
coalesced work. IRQ-tail service wakes `ksmmu-fault` after releasing the IRQ
registry lock. The worker drains at most 32 records per batch under the sleeping
controller mutex, then releases that mutex before logging, rearming routes or
rescheduling. It drains once after enabling IRQs: an event that predates IRQ
enablement is not guaranteed to cause a new edge. Registered routes and worker
context have a permanent owner. Fault processing allocates no queue/domain
metadata. All kernel callers access the controller through the same serialized
operation boundary. It checks containment before returning and prompts the
worker for terminal failures even when hardware raised no IRQ. Unconfirmed
containment stops the host directly, rather than depending on a later worker
wake. Driver operations also check EventQ overflow before admitting work and
while waiting for command completion.

| Condition | Action |
| --- | --- |
| Attributable transaction/configuration fault | Permanently quarantine the StreamID; other streams remain usable. |
| Repeated fault from an already quarantined stream | Consume without repeating the log or invalidation work. |
| Event overflow, impossible queue state, unknown event, out-of-table SID or unexpected stalled transaction | Close the whole controller; missing/uninterpretable fault evidence is not ignored. |
| CMDQ failure, queue-memory abort, interrupt-write error or service failure | Preserve GERROR and command-consumer syndrome, then contain the controller. |
| Command, control-register or IRQ-enable timeout | Close software admission and attempt hardware containment; retain all published backing. |
| Hardware containment cannot be acknowledged | Preserve backing and stop the host; do not continue with an unconfirmed isolation state. |

Stream quarantine invalidates the old STE and synchronizes, installs a valid
abort STE (`CFG=0`) and synchronizes again, then completes any old domain's TLB
invalidation. An abort STE rejects subsequent requests without producing a
fault storm. A preallocated bitmap records quarantine; the existing binding
remains owned, and both detach and reassignment are rejected until reboot.
Already queued fault records are drained normally. This does not reset the
endpoint or authorize reuse of its identifiers for another device.

Controller containment first closes software admission and saves the first
error, offending command words, queue pointers and global syndrome. It disables
interrupt sources, completes the GBPA.ABORT update, then clears CR0 and waits
for CR0ACK. **It never disables translation before establishing disabled-mode
abort policy.** Only after command processing is disabled does it acknowledge
the captured GERROR bits; acknowledging CMDQ_ERR earlier would resume command
execution. A command is never skipped to manufacture successful invalidation.
Acknowledging service failure does not restore a failed controller.

Successful abort acknowledgement blocks further admission but does not prove
physical device/interconnect retirement. Queues, tables and still-mapped pages
remain pinned until reboot, including on failed unmap, partial stream
quarantine, timeout or Drop. No automatic retry/reset/rebind releases them.
An error-returning caller can inspect the retained first-failure snapshot;
further operations return `Failed`. The worker logs a global failure once and
parks. This deliberate terminal state is separate from ordinary denied DMA,
which only quarantines the offending stream.

### Formal I/O runtime notification

A physical-device capability retains its first DMA containment report and a
sticky `DMA_FAULT` signal. The SMMU worker publishes this state after containment,
without depending on a userspace consumer or modifying VM execution state.
Observers are registered before hardware activation and hold only weak references
to device-local notification storage; they never retain a VM or its DMA leases.
Fault handling allocates no observer records.

The formal `io-runtime` queries its adopted storage/network handles, then waits
for new faults and writes one diagnostic per device to its component's stderr
journal stream. `journalctl -c /io-runtime` shows the source, quarantine scope,
SID, event code, available DMA address/direction and original boot timestamp.
Bootstrap has no fault-consumer logic, new handoff messages or journal dependency.
A fault recorded before ownership transfer remains observable by the successor.
Reporting failures do not request a VM stop. Existing kernel serial/ring logging
remains independent of this userspace report.

Inspection requires `INSPECT | ASSIGN_DEVICE`; waiting requires `WAIT`. Records
are non-consuming and contain only the selected device's address evidence.
Controller failures are reported to all registered devices without disclosing
another device's fault address. A device's first report is not replaced by later
controller failure. Consumers stop waiting on the sticky signal once reported;
they cannot acknowledge quarantine or reopen DMA admission through this API.

## Reproduce the acceptance test

From the repository root:

```sh
make test-smmuv3 ARCH=aarch64 QEMU_CPUS=4
make -C kernel -o image test-smmuv3 ARCH=aarch64 QEMU_CPUS=1
```

The first command builds the dedicated `kernel-smmuv3-test` image. The second
reuses that image. This is a correctness test, not a performance measurement;
it does not change default disk/network composition. The runner uses an empty
initramfs, disables PCI IOMMU bypass, owns and reaps its QEMU process, and checks
all isolation markers plus completion of the ordinary kernel self-tests.
Each invocation boots once for a global-error IRQ and once for a command timeout
without an IRQ. Logs are `kernel/target/smmuv3-qemu.log` and
`kernel/target/smmuv3-timeout-qemu.log`. Optional register/page-walk tracing:

```sh
cd kernel
python3 tests/qemu/verify-smmuv3.py qemu-system-aarch64 \
  target/aarch64-unknown-none-softfloat/kernel/hyper.img --cpus 4 \
  --trace target/smmuv3-trace.log
```

Coverage includes real DMA reads/writes, separate domains using the same IOVA,
unassigned streams, attempts to write an unmapped host page, cold and warm
read/write permission denial, fault attribution, warmed-translation revocation,
remapping to a different physical page, stream reassignment, VMID reuse and
stale software identities. Command and event rings both wrap during the test.
The runtime phase first requires a fault produced before IRQ enablement to be
drained. It then issues an unmapped DMA request with IRQs active, without
polling EventQ from the test. It requires an IRQ-driven quarantine, verifies
that previously warmed mappings are also denied, and checks that another device
continues transferring data. The initially unbound stream remains quarantined;
a subsequent 4 KiB failed DMA produces no further IRQ storm. Finally a test-only malformed
command triggers a real GERROR IRQ; the worker must stop the controller, retain
backing, and deny both a warmed IOVA and a direct physical-address probe.
The `--failure timeout` run instead stops CMDQ consumption under the controller
mutex and requires SYNC to time out without any new IRQ. It verifies that the
calling thread wakes the fault worker, which reports the failure, and checks the
same retained-memory and DMA-denial guarantees.

Host-side fault injection separately checks prepublication rollback, partial
stream quarantine, command error syndromes, queue-memory/global error classes,
event overflow (including operations that do not poll EventQ), pending faults
across stream reassignment, malformed firmware routes, MSI target clearing, IRQ-enable
timeout, and failed GBPA/CR0 acknowledgements. It verifies that failed
containment never frees published memory or falsely reports successful
retirement. The QEMU runtime fixture exercises separate wired IRQs; the
combined-route parser has host coverage, not a separate hardware qualification.

### Production I/O VM mapping revocation

```sh
make test-board-dma-revoke ARCH=aarch64 \
  CONFIG_FILE=kernel/configs/qemu_aarch64_defconfig
```

This opt-in test boots the pinned, unmodified Linux I/O VM with the normal
physical virtio-PCI NIC (SID 16) and storage controller (SID 8). It creates a
disposable disk and private Ethernet socket; it does not use external networking.
QMP observes the NIC's receive descriptors. Ordinary Ethernet frames first
demonstrate successful DMA, then the test selects a posted receive buffer on an
already-used page.
The feature-gated kernel worker removes that page from the **production** DMA
domain and completes the ordinary domain TLBI/CMD_SYNC. The mapping owner and a
separate retained reference keep the backing alive; CPU mappings remain intact.

Another Ethernet frame must produce a real SMMU translation fault identifying
SID 16, the removed page and a write access. Every byte in the attempted DMA
write range (virtio header plus Ethernet frame) must remain unchanged. The
unused tail is excluded because Linux may place skb metadata there after
completion; this test preserves CPU access. New storage I/O must still complete
on SID 8, including an observed advance of its physical request queue. Timeout
alone is never success.
Two boots cover faults after formal runtime adoption and before adoption. Both
require a matching `/io-runtime` journal report and a still-running I/O VM;
the early case verifies evidence survives bootstrap ownership transfer.
Serial logs, command receipts and the JSON evidence are kept in the generated
`target/board-tests/dma-revoke.*/` directory.

The runner uses QEMU's experimental `x-query-virtio-*` monitor commands and
QTest physical reads. Its only memory write is an aligned request to a private
test-kernel mailbox while all emulated CPUs are stopped. It does not edit I/O
VM descriptors, payloads, or page tables. The `io-dma-revoke-test` feature is
disabled by default, adds no syscall, and preserves Native startup so that the
ordinary I/O VM can run. This target builds its kernel in
`target/board-tests/dma-revoke-kernel/` and copies it into the disposable fixture;
it does not replace the production `hyper.img` used by `make run`.

Revocation is limited to this disposable boot: the existing fault policy
quarantines the offending SID until reboot, so the test does not restore its
mapping or resume the device. This proves denial of a real NIC DMA write after
revocation, not every possible device/read path or physical DMA retirement.
It complements the standalone read/write/permission tests above.

### QEMU boundary

On the tested QEMU 11.1.2, cached stage-2 permission failures can suppress the
event record: its cached write-fault path can attribute the fault to stage 1,
while cached read denial can be enforced without a fault record. The fixture
reports this explicitly as a QEMU limitation. It still requires correct cold
stage-2 fault records, verifies denied writes leave protected bytes intact,
and checks denied reads cannot disclose the protected pattern through EDU.
It does not change architectural STE fields to accommodate the emulator.
The relevant upstream model paths are
[`smmu_translate`](https://github.com/qemu/qemu/blob/master/hw/arm/smmu-common.c)
and [SMMUv3 fault recording](https://github.com/qemu/qemu/blob/master/hw/arm/smmuv3.c).

QEMU cannot qualify physical interconnect ordering, device reset/drain,
noncoherent DMA or hardware errata. Hardware acceptance must independently
confirm requester routing (including aliases/peer paths), no guest access to
isolation controls, cache coherence, fault delivery under load and retention
through failed revocation. Passing this fixture is not a Pi 5 or complete
I/O VM containment claim.
