<!-- SPDX-FileCopyrightText: 2026 roolrz -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# I/O VM virtio serial transport

The I/O VM boots with an empty PCIe root port. After system init transfers the
running VM to `/svc/io-runtime`, the formal runtime inserts a modern virtio
console endpoint. Linux discovers it through the standard PCIe hotplug driver;
no VM restart, device-tree replacement or sysfs rescan is required.

Stage1 handles the empty bridge's configuration space and transfers its saved
state together with the VM, guest RAM authority and root-port interrupt source.
It contains no virtio-console backend. The formal runtime creates the endpoint
interrupt source after takeover. One worker services the physical-device exits
and the virtual PCI hierarchy, so two consumers cannot race to complete one
vCPU's MMIO request. The [handoff contract](https://hyper-proj.github.io/lib-bootstrap-handoff/docs/handoff.html) describes the
ownership boundary.

## Device and Linux driver contract

The virtual hierarchy is PCI domain 2, with a two-bus ECAM window at
`0x0a600000` and a memory window at `0x0aa00000`. Domain 0 remains available for
assigned physical PCI hardware. The root-port hotplug interrupt uses guest GIC
INTID 120 and endpoint INTB uses INTID 121. The bridge advertises a hotplug-capable
slot, presence and link-change events, and W1C acknowledgement. There is no simulated
power controller, MSI/MSI-X, or hot-removal policy.

The endpoint implements modern virtio-console with `VIRTIO_F_VERSION_1` and
`VIRTIO_CONSOLE_F_MULTIPORT`. Its single named serial port is
`org.qemu.guest_agent.0`; it is not advertised as an `hvc0` console. With MULTIPORT
negotiated, queues 0/1 carry data and queues 2/3 carry control. Without MULTIPORT,
only queues 0/1 are used. The device does not advertise indirect descriptors,
event-index suppression, packed rings, resize or emergency-write support.

The I/O Linux kernel must enable `CONFIG_PCIEPORTBUS`, `CONFIG_HOTPLUG_PCI`,
`CONFIG_HOTPLUG_PCI_PCIE` and `CONFIG_VIRTIO_CONSOLE`, in addition to the existing
PCI/virtio drivers. These are appliance configuration changes in HypeR-io-vm.
The committed QEMU and Pi 5 appliance pins include these drivers.
An older appliance can keep using its existing storage/network devices, but
cannot discover/use this new serial endpoint without those drivers.

SBSA remains the I/O VM's boot and log console. Agent protocol and authorization
live above this transport; see [I/O VM statistics](https://hyper-proj.github.io/io-vm/docs/hyper-integration.html#hotplugged-agent-transport).

## Buffering and authority

The device model lives in `lib/vm-support`; it uses bounded split-ring walks,
copies descriptor metadata before following it, and validates every guest range
against the explicitly delegated RAM extent. The Native adapter uses bounded
GuestMemory copy and aligned acquire/release scalar calls. It cannot use the
VM's imported DMA aliases or a Native block device's reserved queue memory.

Each direction has a 64 KiB byte buffer. Backpressure leaves a descriptor pending
until the peer has room, including descriptors larger than the buffer. Work is
budgeted so a busy console cannot monopolize the MMIO worker. Queue processing
stops when PCI bus mastering or the link is disabled. Invalid descriptors mark
only this endpoint as needing reset and raise a configuration interrupt; they
do not terminate the storage backend VM.

A host consumer explicitly attaches a bounded byte-channel port. Until then the
serial port is host-closed. Host closure discards its buffered session data;
guest port closure or reset after activation invalidates the old host session.
The formal runtime attaches its read-only QGA client through this transport.
Neither device nor agent policy is added to stage1.

## Acceptance

The explicit `test-io-console-hotplug` scenario uses a separate test-only Native
runtime and a Linux helper. It verifies absence before takeover, automatic port
discovery, binary round trips, driver unbind/rebind, and continued Native disk
access. Its overlay disables the normal agent supervisor so the echo helper is
the sole port consumer. It does not change the normal image's stdout.
The test requires an I/O appliance with the kernel options above and an AArch64
Linux build of `tests/qemu/guest/console-hotplug.c`.

On an AArch64 Linux build host:

```sh
clang -static -O2 -Wall -Wextra -Werror \
  tests/qemu/guest/console-hotplug.c -o /tmp/console-hotplug
make test-io-console-hotplug \
  IO_VM_PACKAGE="$(python3 -B scripts/fetch-io-vm.py --platform qemu)" \
  CONSOLE_HOTPLUG_PROBE=/tmp/console-hotplug
```

The probe is a disposable test payload; production packages do not include it.
