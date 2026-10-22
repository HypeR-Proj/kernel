<!-- SPDX-FileCopyrightText: 2026 roolrz -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# I/O authority and DMA memory ownership

The default QEMU deployment assigns modern virtio-PCI SCSI and network
functions behind an SMMUv3. The kernel binds each function's firmware-resolved
requester ID to the I/O VM's DMA domain before enabling bus mastering. Only
that VM's RAM and explicit live backend grants enter the domain. This protects
ungranted HypeR memory; it does not protect a guest from its authorized I/O
backend or prevent denial of service.

Physical virtio-mmio remains supported by a dedicated compatibility deployment
and test. It does not traverse QEMU's PCI SMMU. That deployment and Pi 5 remain
trusted-I/O-VM configurations; neither gains DMA containment from capability
attenuation alone.

## Device operations

An assignment authority separates firmware inspection (`INSPECT`), physical
claims (`ASSIGN_DEVICE`) and VMO physical-address disclosure (`MAP_DMA`). An
inspection-only duplicate cannot acquire a device or inspect a VMO's physical
extent. Resolving a DMA extent also requires `READ|MAP` on the supplied VMO; it
does not install a DMA mapping or certify device quiescence.

Each successful claim owns a concrete PhysicalDevice kobject and its exact
firmware-derived resources. Binding it requires `ASSIGN_DEVICE`; userspace
register access requires `READ` or `WRITE`; IRQ sequence observation requires
`WAIT`; completion/rearm requires `ACK_INTERRUPT`. A register read can have
hardware side effects, so it remains an operation on an active assignment.

Init grants io-runtime the three authority operations explicitly in the service
manifest. A generic register/IRQ worker receives only the four operation rights
it needs on its PhysicalDevice. It cannot duplicate that handle or assign it to
another VM through that handle. Workers share io-runtime's process and handle
table, so attenuation is not a security boundary between those threads.
Linux receives mediated device resources, not a Native authority
handle. Existing handle attenuation, generational KOIDs, exact register-window
checks, exclusive resource claims and host-owned PCI/MSI mediation still apply.
See the [physical-device ABI](../../../sdk/abi/docs/physical-devices.md).

## Backend memory lease

`BackendMemoryLease` is a kernel-only kobject, visible by kind/KOID to authorized
object inspection but never publishable as a userspace handle. Its initial
owner uses the existing `KernelService` reference class. Every cloned owner
retains the stable VMO pages and exclusive hardware-write lease. The object and
its page-address snapshot are charged to the creating resource domain.

Production live backend mappings and SMMU mappings retain this lease. Each DMA
extent holds the same owner independently of the CPU mapping or source handles.
Closing a handle or removing a CPU mapping cannot release a page still held by
the DMA domain. Final destruction uses the ordinary object reaper and performs
no device register access, reset, or IOMMU command.

For protected PCI assignments, live removal withdraws CPU mappings and completes
DMA invalidation before releasing the lease. Full VM retirement first resets
the physical virtio functions and waits for their status-zero acknowledgement,
then detaches streams, invalidates mappings and destroys the domain. Failures
retain the backing rather than treating a backend reply as a physical fence.
Unprotected MMIO and Pi 5 paths still require a trusted quiescence protocol.

## QEMU physical PCI transport

The generic ECAM host admits modern virtio-SCSI (`1af4:1048`) and virtio-net
(`1af4:1041`) single-function endpoints. Physical BAR placement, configuration
and requester identity remain host-owned. Linux sees a separate mediated host
bridge for each function and owns all SCSI and network protocol handling.
Readonly configuration capabilities expose only modern virtio transport fields;
MSI/MSI-X, power management, ATS and PCI_CFG access stay hidden.

Interrupts use exclusive firmware-routed INTx lines, with guest acknowledgement
rearming the physical level interrupt. No interrupt-controller page is mapped
into the DMA domain. MSI-X and shared INTx lines are outside this implementation.
The domain uses identity IOVAs for its authorized physical extents, matching
the existing guest `dma-ranges` contract without granting access to other RAM.
All assigned functions share the I/O VM's domain. Production grants currently
permit both reads and writes; they do not partition shared memory by device or
individual I/O request.

## SMMUv3 data and control memory

The driver distinguishes exclusive coherent control allocations (`DmaMemory`)
from retained guest/shared payloads (`DmaBuffer`). Only the former exposes a CPU
address for table and queue operations. The payload contract provides a stable
length and physical page addresses, allowing noncontiguous RAM without giving
the driver direct access to its bytes.

`map_buffer` validates the complete IOVA/physical range, excludes overlap and
allocates intermediate tables before publishing any leaf. A single owner covers
the complete buffer. A single completed domain invalidation covers publication.
An allocation failure may leave empty intermediate tables, which grant no DMA
access. A command failure after publication retains the buffer in the failed
controller; an error does not imply rollback.

`unmap_buffer` accepts the original mapping base and revokes the complete range.
It returns the owner only after TLBI and CMD_SYNC complete. A timeout retains the
owner even if leaf entries have already been cleared. Page helpers accept only
one-page buffers. Revoked IOVA reuse still requires the existing device and
stream lifecycle rules; this API is not a device reset.

The real QEMU PCI EDU fixture now maps VMO-backed leases after dropping their
original VMO/backing owners. It checks both DMA directions, host guard pages,
different domains, permissions, revocation/reassignment and fault/timeout
quarantine. Host fault-injection tests additionally cover scattered buffers,
cross-table ranges, overlapping mappings, allocation rollback and failed
revocation without premature owner release. They walk the actual stage-2 leaf
entries after partial allocation failure and an invalid final physical page,
and check that failed revocation clears leaves while retaining their payload
owners. See [SMMUv3](smmuv3.md).

Production virtio-PCI storage and networking have functional acceptance tests.
The separate [physical NIC revocation fixture](smmuv3.md#production-io-vm-mapping-revocation)
also exercises the production assignment and grant path: it warms a Linux RX
buffer through successful DMA, withdraws that page's mapping, and checks that
the next device write faults without changing the transfer's destination bytes.
Storage DMA continues, and formal io-runtime reports the fault through journal
without stopping the I/O VM. Faults both before and after bootstrap handoff are
covered. This does not cover every descriptor/device/read path or establish
physical DMA retirement on hardware.

## Pi 5 coverage and remaining integration

Host ownership now reserves every firmware translation provider identified by
`#iommu-cells`, including BCM2712 IOMMU controls, even when no driver is bound.
The catalogue's existing page/IRQ alias checks also exclude aliases of those
controls from generic userspace claims. Reserving controls is necessary but is
not evidence that every RP1 master passes through that controller.

Raspberry Pi's [Pi 5 device tree](https://github.com/raspberrypi/linux/blob/rpi-6.18.y/arch/arm64/boot/dts/broadcom/bcm2712-rpi-5-b.dts)
connects selected RP1 display/camera masters to `iommu5`; that association does
not establish Ethernet coverage. The [SoC device tree](https://github.com/raspberrypi/linux/blob/rpi-6.18.y/arch/arm64/boot/dts/broadcom/bcm2712.dtsi)
describes SDIO1/SDHCI separately and does not give it an `iommus` association.
No verified hardware containment for that path has been established here.
The [RP1 peripheral specification](https://datasheets.raspberrypi.com/rp1/rp1-peripherals.pdf)
must be read alongside the host bridge and SoC routing: a translation mechanism
covering selected masters cannot be assumed to cover the whole PCI function.

The current BCM2712 inbound DMA windows provide address translation over broad
RAM ranges. They have not been converted into per-owner isolation windows or
proven to supply the required retirement fence. No speculative programming of
those live physical windows or SDHCI DMA controls is included in this change.
SDHCI remains the explicit trusted exception; HypeR does not add an SD driver.

Pi 5 requires verified coverage and retirement for its applicable bridge/IOMMU
paths before a deployment can claim I/O VM containment. Future physical
qualification should use hardware with a documented DMA isolation boundary.
