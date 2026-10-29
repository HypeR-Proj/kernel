<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# Raspberry Pi 5 bring-up

HypeR supports the host GICv2 MMIO interface and adapts its PL011 driver for
Pi 5's dedicated three-pin debug UART, including its smaller AXI register window.
A four-core Native shell with timer-driven scheduling and interrupt-driven input
has been validated.
Native boot, bidirectional UART, SD-backed `/data` reads and basic Alpine
SMP/reboot/poweroff with file persistence were confirmed on a Pi 5 D0 board.
The combined SDIO1/RP1 deployment also completed the 2026-10-06
[storage and TCP performance survey](https://hyper-proj.github.io/HypeR/index.html#exploratory-pi-5-io-measurements).
See the qualification records below; sustained load, power-loss durability
and DMA retirement under faults remain unqualified and are
[deferred TODOs](https://hyper-proj.github.io/HypeR/docs/project/roadmap.html#deferred-hardware-qualification).

GICv2 guest virtualization is implemented and tested with Linux on QEMU.
Firmware must describe the GICH and GICV register windows plus the maintenance
PPI in the GIC node. Hosts without those resources retain Native application
support and reject VM admission. The current backend requires five priority
and preemption bits and supports up to 64 list registers.

Each vCPU owns its saved GICH control, VMCR, APR and list-register state. Guest
stage-2 maps the first 4 KiB of the banked GICV CPU interface as Device
memory. The second page, containing DIR, traps for software deactivation so
active interrupts outside the hardware list registers can also be retired.
GICC and GICH remain host-only. The emulated distributor exposes 1..8 vCPUs and 256 interrupt
IDs using GICv2 without security extensions. The Native platform-info query reports the
GIC revision so vm-runtime emits matching guest firmware. GICv3 hosts retain
their existing guest profile.

QEMU covers guest boot, timer/console wakeups and VM retirement, including
runtime crashes. Physical host and diskless Linux boot are confirmed. Interrupt ordering under
load and physical device quiescence still need hardware validation. The Linux I/O
backend has a board-configured deployment path; its Pi 5 hardware qualification
remains in the [roadmap](https://hyper-proj.github.io/HypeR/docs/project/roadmap.html). RP1 UARTs on the 40-pin header are not the
console used by this configuration.

The I/O appliance is built and published by the separate HypeR-io-vm repository.
HypeR consumes its pinned GHCR package and provides the guest DTS/DTB and
launch configuration. The firmware-provided host DTB used below is not a guest
device-assignment description. The lock selects separate QEMU and Pi 5 build
profiles. The Pi profile includes the upstream storage drivers; its package
hardware-qualification status is separate from the recorded development runs. See
the [I/O VM contract](https://hyper-proj.github.io/io-vm/docs/hyper-integration.html) and
[board storage deployment](https://hyper-proj.github.io/build/docs/deployment/board-storage.html).

The Native I/O runtime validates the C0/D0 SDIO1 dependency graph and runs a
separate userspace MMIO/IRQ worker. The kernel supplies immutable firmware
facts, atomic exclusive resource claims, bounded register access and IRQ
notification/completion; it contains no SDHCI register or pinmux policy.
Retiring an I/O VM cannot yet prove SD DMA has stopped, so the device and all
own/imported DMA backing remain quarantined until host reboot. QEMU's generic
device test does not establish physical SD reset, cache or DMA correctness.

## Recommended boot chain

Use the official Pi 5 EEPROM firmware and its built-in BL31 to load HypeR
as an AArch64 Linux Image at EL2. Leave `armstub` unset: no custom TF-A binary
is built or installed. BL31 supplies PSCI; HypeR requires FEAT_VHE.
The [official EEPROM release notes](https://github.com/raspberrypi/rpi-eeprom/blob/master/firmware-2712/release-notes.md)
track the bundled BL31 updates. Record the installed EEPROM version; the disk
image does not pin or update EEPROM.

## Native-only image integration

HypeR downloads official prebuilt DTBs and overlays using
`scripts/rpi5-boot.lock.json`, including SHA-256 checksums and a pinned
corresponding Linux source archive. Python 3.11+ is required; no firmware
compiler or separate boot-build repository is needed. Downloads are cached
under `target/rpi5-boot/` and verified before reuse. External sources and binaries
remain outside HypeR Git and retain upstream licenses.

From a HypeR checkout:

```sh
make rpi5-bringup ARCH=aarch64
```

`RPI5_BOOT_PACKAGE=/path/to/package` remains an optional local development
override for a verified v2 package. Old custom-TF-A v1 packages are not accepted.

This builds the same architecture-wide kernel/application binaries and a
Native-only initramfs using `bootstrap/stage1/init/config/services-console-only.json`.
It validates every external package checksum, then creates
`target/board/rpi5-native/disk.img` from `boards/rpi5-native.json`. The image has
a 64 MiB FAT32 GPT boot partition; no Linux I/O VM, Alpine download or VM disk
is needed to qualify the kernel and shell. The unused device selector in this
board policy records the later storage assignment; Native-only init does not
start the I/O service or claim the SD controller. This is separate from the
full `boards/rpi5.json` deployment with its 1 GiB configuration volume and guest
disks. The full deployment has basic storage/network results; extended
qualification remains deferred.

Existing image outputs are refused. To explicitly replace this generated
image, add `BOARD_IMAGE_REPLACE=--replace`. This command never writes a physical
device. Preserve the working Pi OS medium and flash a spare medium with an image
writer after checking its device identity. Keep `manifest.json`,
`boot-notices.txt` and `sources.tar.gz` with any redistributed image; verify
upstream license obligations for the particular distribution.

The generated FAT partition contains these settings:

```ini
[pi5]
kernel=hyper.img
initramfs bootstrap.cpio followkernel
cmdline=cmdline.txt
uart_2ndstage=1
camera_auto_detect=0
display_auto_detect=0
[all]
```

`cmdline.txt` selects `earlycon=pl011,0x107d001000,115200n8 loglevel=7`.
No fixed BL33 or DTB load addresses are configured. EEPROM firmware patches the
host DTB for actual board revision, RAM size and reserved memory; the built-in BL31 supplies
PSCI and EL2 handoff. Record the installed EEPROM version alongside the boot
package manifest. Never substitute QEMU's DTB or a guest-assignment DTB.

The firmware's [config.txt reference](https://www.raspberrypi.com/documentation/computers/config_txt.html)
describes Image and initramfs placement. `initramfs` deliberately has no `=`.
HypeR reads the uncompressed newc archive directly. Firmware must describe its
bounds in `/chosen/linux,initrd-start` and `linux,initrd-end` and retain BL31's
reserved-memory range. Connect the dedicated debug port at **115200 8N1**.
The PL011 driver retains firmware's clock and baud setup. The GPIO14/15 RP1
UART is not an alternative early console for this image.

## Hardware discovery and mapping

The [Pi 5 device tree](https://github.com/raspberrypi/linux/blob/rpi-6.18.y/arch/arm64/boot/dts/broadcom/bcm2712-rpi-5-b.dts)
and its BCM2712 includes provide the hardware addresses. HypeR translates bus
`ranges`; the interrupt driver does not hardcode these addresses:

| Resource | Physical address / interrupt |
| --- | --- |
| GIC distributor | `0x107fff9000` |
| GIC CPU interface | `0x107fffa000` |
| Dedicated debug PL011 | `0x107d001000`, SPI 121 / INTID 153 (C0/C1); SPI 120 / INTID 152 (D0) |
| EL2 physical timer | PPI 10 / INTID 26 |
| CPU power | PSCI, SMC conduit |

Before enabling the MMU, allocation-free FDT discovery builds the temporary
identity map from firmware RAM. It maps RAM as Normal memory and other blocks
as Device/XN, including peripherals above 4 GiB. This transient map covers
512 GiB with 1 GiB blocks and rejects RAM/MMIO sharing a block or resources
outside that range. The permanent map remains page-granular and applies the
usual RAM, reserved-memory and MMIO validation. No Pi-specific memory addresses
are embedded in the bootstrap.

## Validation

```sh
make test-native-gicv2 QEMU_CPUS=4
make test-native-gicv2 QEMU_CPUS=1
make test-guest-smp QEMU_MACHINE=virt,virtualization=on,gic-version=2
```

CI runs both configurations, exercising Native services, std programs, shell
input and userspace-managed Linux guest startup. It also runs four-core GICv2
kernel self-tests, paced guest-console input and vm-runtime crash recovery.
Host tests cover guest distributor decoding, LR encoding, delivery gates,
pending-state preservation, GIC SGI sender preservation,
CPU target masks, UP target-register behavior, spurious IDs, FDT matching,
nine-window devices, and low-RAM/high-MMIO mapping. The upstream Pi 5 DTB was
also checked locally for generic and essential-device discovery.

Native boot to the shell and bidirectional debug-UART input were confirmed on
a Pi 5 D0 board on 2026-09-20 after installing the stepping overlay. This does
not qualify the guest or physical storage paths.

On hardware, record firmware and HypeR logs, verify four online CPUs and the matching
console IRQ (INTID 153 for C0/C1, 152 for D0), then exercise slowly typed shell
commands, idle wakeup,
and repeated `top` entry/exit. QEMU passing is not a substitute for these checks.

After host bring-up, use a four-vCPU guest image and verify all guest CPUs are
online, then repeatedly offline/online secondary CPUs through Linux sysfs.
Exercise per-CPU timer wakeups, SGI/IPI traffic, SPI routing, and console input
under concurrent load. Repeat guest `reboot` and `poweroff`, verify that
HypeR itself stays running, and check guest pages return to the stopped baseline.
Also terminate vm-runtime during a pending power request and with CPUs powered
off; every configured Thread and translation must retire before page reuse.
Include VMID rollover while guests run and resume on another CPU, checking that
new epochs flush old translations before recycled tags enter hardware.
Record cold/warm boot logs and firmware-provided GICH/GICV/maintenance resources.
Real hardware must validate cross-core cache publication, TLB invalidation,
interrupt ordering, and device quiescence; QEMU cannot establish those properties.
Guest suspend is not supported. See [VM power and SMP](../design/vm-bundle.md#guest-power-requests-and-smp).

### Firmware handoff and console constraints

The Pi 5 packer pads the staged kernel to its Image-header runtime size so
`followkernel` cannot place initramfs in its NOLOAD BSS, tables or boot stack.
The original build artifact is unchanged.

The firmware may add nonstandard device metadata such as `/axi/vc_mem`.
Malformed device `reg`, `interrupts`, or `ranges` encodings are quarantined per
node: no MMIO or interrupt capability is published from that node. A malformed
bus translation also quarantines descendant resources. Valid generic resources
remain discoverable for userspace drivers even without a kernel driver.
Recognized essential controllers/timers reject quarantined resources, as does
platform driver binding. RAM, CPU and reserved-memory errors remain fatal;
structural FDT bounds checks are never bypassed. No Pi-specific node-name
exception is used.

BCM2712 describes its debug PL011 with a 512-byte `reg` window. Early console
validation and permanent binding use the driver's operational register extent,
not a 4 KiB page or the optional PrimeCell identification window. Page rounding
belongs to the mapper and does not enlarge the firmware-granted resource.

The Native return frame retains the full 64-bit HCR_EL2 in ordinary stack
storage, as the guest return path does. ESR_EL2 is not a general-purpose scratch
register: reserved upper syndrome bits cannot preserve HCR_EL2.E2H at bit 34.
An emulator that retains those bits can conceal a loss of the VHE translation
regime immediately before EL0 entry on hardware. The Native boundary check
rejects reintroducing this scratch-register use.

Console and kernel log output preserve LF line endings without inserting CR.
Configure the serial terminal to display LF as a new line at column zero if
needed. Interactive input accepts CR, LF and CRLF and normalizes them to LF.

The boot partition includes both `bcm2712-rpi-5-b.dtb` and
`bcm2712d0-rpi-5-b.dtb`, plus `overlays/bcm2712d0.dtbo` and
`overlays/overlay_map.dtb`, from the same pinned vendor firmware release.
Leave `device_tree` unset and do not force the D0 overlay on all boards.
Firmware can load the base DTB and automatically apply the D0 stepping overlay;
supplying a separate D0 DTB alone is not sufficient for that boot path.
The overlay changes UART10 from SPI 121 (INTID 153) to SPI 120 (INTID 152), along
with other silicon differences. Missing it can leave a working shell prompt
with no UART RX interrupts. These differences belong in vendor device-tree
inputs, not kernel IRQ overrides. All DTB/overlay binaries and corresponding
sources remain in the external boot package.

The FAT32 boot/configuration volume uses the GPT Basic Data type, not the EFI
System Partition type. Its volume label is `HYPER`; firmware files and HypeR
configuration continue to share this volume. Flash the complete `disk.img` to
the whole device, then eject and reconnect it for the host to discover the new
partition table. No UEFI boot manager is involved.

The default EEPROM-stub image was subsequently confirmed working on hardware
on 2026-09-20. Keep the installed EEPROM version with qualification records.

## Diskless stage-1 recovery

`make rpi5-bringup ARCH=aarch64 BOARD_IMAGE_REPLACE=--replace` packages the
kernel and static bootstrap recovery environment with pinned firmware. It runs
stage-1, raw console and fallback-sh only. Without an independent system volume,
it cannot start formal init, the VM manager or ordinary tools. It does not
embed a formal system inside the boot archive.

For the complete I/O VM, storage and network deployment use `make rpi5-sd` below.
Separate QEMU and Pi 5 appliances are published from the same pinned Linux source;
`scripts/io-vm.lock.json` selects immutable platform digests. `IO_VM_PACKAGE`
remains an optional development override for that complete deployment.

## SD-card backend qualification

`boards/rpi5.json` selects the physical BCM2712 SDIO1 controller by firmware
path. The default split layout contains a 64 MiB read-only EROFS system
partition, a 1 GiB FAT configuration partition and a 2 GiB Alpine ext4 root
partition, matching the QEMU deployment layout. Alpine uses the minimal rootfs and
remains stopped until `vmm start alpine`. Its FIT lives at `/data/vm/alpine.itb`,
not in the Native initramfs. Its root disk is exported separately through
dm-linear and vhost-scsi; use `vmm console alpine` after starting it.
Build its complete flash image with a matching protocol version 3 Pi 5 I/O
appliance, official boot inputs and Alpine downloads:

```sh
make rpi5-sd ARCH=aarch64
```

The output is `target/board/rpi5-native/disk.img`, matching the earlier bring-up
path. The build imports the pinned Pi 5 appliance; `IO_VM_PACKAGE` optionally
selects a complete local boot package. Add `BOARD_IMAGE_REPLACE=--replace` to
replace an existing generated image. Flashing it to the whole SD card replaces
existing partitions and data.
The build itself only writes this generated image, never the physical card.

Native io-runtime claims the SDIO1 dependency graph and supplies a guest DT;
Linux drives SDHCI, GPIO and pinctrl. HypeR io-runtime reads and validates the physical disk GPT, selects the
`hyper-data` partition using the board policy, and authorizes its exact range.
The appliance creates a client-scoped dm-linear mapping without parsing GPT.
Linux exports the Native block volumes through LIO/vhost-scsi. Native fs-backend
applies fstab and starts separate workers to mount EROFS at `/system` and FAT at
`/data`; Linux must not mount those filesystems concurrently.
The boot files remain ordinary firmware-readable files in the configuration
FAT volume.

Bootstrap readiness is `[HypeR bootstrap stage 1][io-runtime] ready; bootstrap mounts complete` (after the boot timestamp),
after fs-backend has mounted `/system` from the RAM root's fstab. System init
starts after stage-1 switches the system filesystem to `/`, adopts core
supervision, and requests `/etc/fstab`.
The later `FAT mounted at /data` and fleet-configuration result precede the shell;
bootstrap readiness alone does not mean that data is mounted. Check `mount`,
`vmm status io`, `ls /data`, and a small file write/read. See
[boot layout](https://hyper-proj.github.io/bootstrap/docs/boot-layout.html) for the current handoff and retirement.
Earlier configuration-volume mounting and directory reads passed on Pi 5 D0;
those results do not qualify later namespace or provider-adoption changes.

The 2026-09-22 hardware run also verified a 256 MiB, two-vCPU Alpine guest
with a 128 MiB resident I/O VM: both CPUs online, three CPU 1 off/on cycles,
one ordinary `reboot`, and one ordinary `poweroff`. A file written and synced
on the SD-backed ext4 root survived the reboot. After poweroff, Alpine reached
`stopped`, the I/O VM remained `running`, `/data` remained readable, and the
backend reported successful memory release. That run predates the combined
SDIO1/RP1 deployment and establishes basic guest lifecycle operation.

On 2026-10-06, the combined deployment using the published I/O VM pin completed
1 GiB Native FAT and Alpine ext4 writes/overwrites with full readback, plus
4 KiB synchronous-write measurements. Native Raspbian on a separate card
provided reference data. These were single-round exploratory measurements;
same-boot readback can hit caches and does not prove power-loss durability.
See the [results](https://hyper-proj.github.io/HypeR/index.html#exploratory-pi-5-io-measurements) and
[manual test image/tools](https://github.com/HypeR-Proj/ci/blob/main/hardware/storage/README.md).
The tools are absent from default builds and images.

Stress, power-cut and physical retirement trials remain deferred and
unqualified. The kernel retains generic device ownership and notification
mechanisms; SDHCI policy remains in userspace.

## Ethernet backend qualification

`boards/rpi5.json` includes the built-in RP1 Ethernet controller in the SD-card
deployment. Its `alpine` guest has both a root disk and a network interface.
The external LAN must provide DHCP and routing. HypeR Native keeps its network
endpoint reserved; it does not establish a network link.

The I/O VM creates `hbr0`, adds its RP1 `eth0` uplink and brings them up during
startup. Each guest TAP joins the bridge when its virtio-net device activates.
No manual bridge configuration is needed. The appliance reports
`HypeR I/O: network default ready on eth0` after bridge setup; this does not
establish that the cable, PHY, external switch or DHCP server is working.

The pinned protocol version 3 Pi 5 appliance includes upstream
PCI host-generic, GICv2m, RP1 PCI/MFD, MACB, Broadcom PHY, RP1 clock and GPIO
drivers. The build imports its immutable GHCR digest automatically:

```sh
make rpi5-sd ARCH=aarch64
```

The resulting `target/board/rpi5-native/disk.img` is a complete SD image. Add
`BOARD_IMAGE_REPLACE=--replace` only when deliberately replacing an existing
generated image. The board generates `pciex4_reset=0` in
`config.txt`, using the official firmware's
[PCIe configuration handoff](https://github.com/raspberrypi/documentation/blob/master/documentation/asciidoc/computers/config_txt/boot.adoc).
It retains the existing [AArch64 Image boot requirement](https://docs.kernel.org/arch/arm64/booting.html)
that firmware quiesce all DMA-capable devices before entering HypeR. Preserving
PCIe configuration does not waive that requirement. The kernel checks the
actual link and BARs, then establishes inbound DMA mapping before admitting RP1.
Firmware may leave the live root port's PCI bus numbers unset. HypeR configures
primary bus 0 and secondary/subordinate bus 1 in that case, then verifies the
write before accessing endpoint configuration space. Valid firmware numbering
is retained; address windows and device-specific RP1 setup are not changed by
this enumeration step.

DT supplies the bounded CPU MMIO reservation, not a snapshot of firmware's PCI
BAR assignments. HypeR translates each probed BAR through the live BCM2712
outbound windows and requires the whole extent to fit that reservation. A
window need not cover unused space in the reservation; separate windows may
cover different BARs. Overlapping CPU decoding, ambiguous BAR translations and
out-of-reservation extents remain rejected.

If firmware placed the only window covering all BARs outside that reservation,
HypeR relocates its CPU side while preserving its PCI target and endpoint BARs.
For example, firmware may map PCI `0x80000000` at CPU `0x1c00000000`, while the
early MMIO reservation starts at `0x1f00000000`. Relocation decodes only the
MiB-rounded prefix needed to reach all BARs, within the reservation's bound.
Conflicting windows or a BAR span larger than the reservation are rejected.
The host gates bus mastering, disables the old decode, then programs and reads
back the destination before accessing any function MMIO. Intermediate decodes
cannot extend beyond the old or final window.

Inbound DMA setup belongs to the PCI host, rather than being a prerequisite
left to the bootloader. While endpoint bus mastering is disabled, HypeR closes
all ten old inbound decoders and installs the validated `dma-ranges` from DT.
The supported layout contains the fixed RAM translation at PCI address 64 GiB,
the MIP MSI page, and optional loopback ranges inside the owned MMIO aperture.
Unowned SoC register targets, additional RAM aliases and overlapping PCI ranges
are rejected before register access. Each configuration write is read back;
the function is not published on failure. This is address translation for the
trusted I/O VM, not DMA isolation.

If init reports storage readiness failure, first inspect the earlier kernel
log for `PCI function handoff unavailable` and inspect the forwarded I/O VM
messages in the captured serial output. The recovery shell and `mount` remain
available even if storage cannot be mounted; ordinary `/bin` tools need
a mounted independent system volume and successful root handoff. A link failure records the root-port
MMIO address, PCIe status, bus numbers and control register before any endpoint
access. This distinguishes an inactive PCIe link from rejected bus-number
programming; neither diagnostic refers to the Ethernet cable or DHCP.
An `Outbound` failure includes the BAR index, bus address, size, CPU aperture
base and all four raw outbound-window register sets. `BarType`, `BarSize`,
`BarPlacement` and `BarOverlap` identify endpoint sizing and placement failures
separately. Retain the complete diagnostic, including wrapped serial lines.
An `OutboundWrite` failure identifies a rejected relocation register write,
including its expected value, observed value and comparison mask.
A `Dma` failure now records the setup stage, register offset, expected value,
observed value and comparison mask. Successful preparation reports
`PCI function 1de4:0001 prepared; DMA windows configured`. The I/O VM's later
activation controls when endpoint bus mastering can actually be enabled.

The whole RP1 PCI function belongs to the I/O VM. HypeR owns the physical
BCM2712 PCIe bridge, BAR extents and MSI routing; Linux owns RP1's interrupt
controller and all enabled peripheral drivers. Guest firmware describes a
generic ECAM host and GICv2m MSI frame plus the original RP1 subtree. The
firmware's BCM2712 debug UART remains the host console. RP1 peripherals cannot
be split between independent owners through this assignment.

The current host transport supports one firmware-initialized, single-function
endpoint with memory BARs and MSI-X. There is no IOMMU DMA isolation: this is a
trusted I/O VM deployment, not safe passthrough to an untrusted guest. After
activation, I/O VM teardown retains its device claims and RAM until host reboot
because physical DMA retirement is not yet qualified.

The 2026-10-06 physical survey exercised guest TCP in both directions with one
and four connections, using the Mac as client and the Alpine guest as server.
Native Raspbian was measured separately with the same client. See the
[results and limits](https://hyper-proj.github.io/HypeR/index.html#exploratory-pi-5-io-measurements) and
[manual network exercise](https://github.com/HypeR-Proj/ci/blob/main/hardware/network/README.md).
This does not qualify simultaneous disk/network load, link recovery or faults.

For manual functional checks, connect Ethernet to a network with DHCP, wait
for `/data`, then start Alpine and enter its console:

```text
vmm start alpine
vmm console alpine
```

Alpine loads virtio-net and starts DHCP automatically after mounting its root.
The client keeps running to renew its lease; an unavailable DHCP server does
not delay the shell. Inside the guest, inspect its address and route:

```sh
ip addr show eth0
ip route
cat /var/log/udhcpc-eth0.log
```

The following extended checks remain deferred qualification work. When a test
peer is available, verify downloads against a known SHA-256 and upload a
file to compare its hash at the receiver. For multi-guest qualification, add a
second guest with a distinct MAC address to the board configuration, rebuild
the test image, and repeat with both guests running.
Detach the console with `Ctrl+]`, then `d`. Repeat guest stop/start and network
driver unbind/rebind while the other guest transfers data; also verify a synced
file on Alpine's SD-backed root and Native `/data`. Check that each guest stop
receives `RELEASE_MEMORY: ok`. Finally exercise cable disconnect/reconnect and
repeated cold boots. Record the firmware version and C0/D0 board revision with
the results. These checks are manual hardware qualification, not QEMU CI or a
performance baseline.

## Release boundary

These Make targets are local integration tools. The separate image distribution
repository pins merged HypeR; HypeR's locks select the I/O VM and official boot
inputs, including immutable binary and source artifacts; see [image distribution](https://hyper-proj.github.io/pi5-images/docs/distribution.html).
Do not promote local reassemblies or uncommitted builds to release pins.

On AArch64, stage-2 permission faults outside a stage-1 walk do not provide a
generally reliable HPFAR IPA. The exception backend recovers it with a guest
stage-1 address translation, preserving PAR and restoring the host HCR before
accessing host memory. This fixes first execution of Linux userspace pages on
Pi 5; an unresolved instruction abort is a memory fault, never an MMIO request.
