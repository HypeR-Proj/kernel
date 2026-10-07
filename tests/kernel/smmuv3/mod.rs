// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real PCI DMA acceptance under QEMU `virt,iommu=smmuv3`.

mod edu;
pub(crate) mod runtime;

use crate::kernel::device::iommu::{CoherentMemory, KernelEnvironment as E};
use hyper::drivers::{
    iommu::smmuv3::{
        Controller, DmaMemory, Environment, Error, Permissions, firmware::pci_stream_id,
    },
    platform::{DriverServices, PlatformDevice},
};

const IOVA: u64 = 0x2000_0000;
const OTHER_IOVA: u64 = 0x8000_1000;
const PATTERN_A: u64 = 0x1937_4567_89ab_cdef;
const PATTERN_B: u64 = 0xfedc_ba98_7654_3210;
const GUARD: u64 = 0xd0d0_d0d0_d0d0_d0d0;

pub(super) struct Fixture {
    devices: [edu::Edu; 3],
    streams: [u32; 3],
}
impl Fixture {
    fn prepare(
        smmu: &PlatformDevice,
        nodes: &[PlatformDevice],
        services: &dyn DriverServices,
    ) -> Result<Self, Error> {
        let phandle = word(smmu.property("phandle").ok_or(Error::Unsupported)?)?;
        let mut hosts = nodes.iter().filter(|node| {
            node.is_compatible("pci-host-ecam-generic") && node.property("iommu-map").is_some()
        });
        let host = hosts.next().ok_or(Error::Unsupported)?;
        if hosts.next().is_some() || host.property("dma-coherent").is_none() {
            return Err(Error::Unsupported);
        }
        let map = host.property("iommu-map").ok_or(Error::Unsupported)?;
        let mask = host
            .property("iommu-map-mask")
            .map(word)
            .transpose()?
            .unwrap_or(u32::MAX);
        let [a, b, unbound] = edu::Edu::discover(host, services)?;
        let sid_a = pci_stream_id(map, mask, a.requester, phandle)?;
        let sid_b = pci_stream_id(map, mask, b.requester, phandle)?;
        let sid_unbound = pci_stream_id(map, mask, unbound.requester, phandle)?;
        if sid_a == sid_b || sid_a == sid_unbound || sid_b == sid_unbound {
            return Err(Error::Unsupported);
        }
        crate::pr_info!(
            "HypeR test: SMMUv3 PCI routing: {:04x}->{} {:04x}->{} {:04x}->{}",
            a.requester,
            sid_a,
            b.requester,
            sid_b,
            unbound.requester,
            sid_unbound
        );

        Ok(Self {
            devices: [a, b, unbound],
            streams: [sid_a, sid_b, sid_unbound],
        })
    }
}

pub(crate) fn run(
    controller: &mut Controller<E>,
    smmu: &PlatformDevice,
    nodes: &[PlatformDevice],
    services: &dyn DriverServices,
) -> Result<(), Error> {
    let Fixture {
        devices: [mut a, mut b, mut unbound],
        streams: [sid_a, sid_b, sid_unbound],
    } = Fixture::prepare(smmu, nodes, services)?;
    let domain_a = controller.create_domain()?;
    let domain_b = controller.create_domain()?;
    let page_a = page(PATTERN_A)?;
    let page_b = page(PATTERN_B)?;
    let guarded = page(GUARD)?;
    let address_a = page_a.virtual_address();
    let address_b = page_b.virtual_address();
    controller.map_page(domain_a, IOVA, page_a, Permissions::ReadWrite)?;
    controller.map_page(domain_b, IOVA, page_b, Permissions::ReadWrite)?;
    // Unconfigured StreamID must not reach physical RAM, even with a valid PA.
    unbound.probe_write(guarded.physical())?;
    fault(controller, sid_unbound, 4, None)?;
    verify(guarded.virtual_address(), GUARD)?;
    controller.attach(sid_a, domain_a)?;
    controller.attach(sid_b, domain_b)?;
    if controller.attach(sid_a, domain_b) != Err(Error::StreamBusy) {
        return Err(Error::Corrupt);
    }

    // Real device reads into the EDU buffer, then real writes into mapped RAM.
    a.read_dma(IOVA)?;
    b.read_dma(IOVA)?;
    fill(address_a, GUARD);
    fill(address_b, GUARD);
    a.write_dma(IOVA)?;
    b.write_dma(IOVA)?;
    clean(controller)?;
    verify(address_a, PATTERN_A)?;
    verify(address_b, PATTERN_B)?;
    crate::pr_info!("HypeR test: SMMUv3 bidirectional DMA and separate domains passed");

    a.probe_write(guarded.physical())?;
    fault(controller, sid_a, 0x10, Some((guarded.physical(), false)))?;
    a.probe_read(OTHER_IOVA)?;
    fault(controller, sid_a, 0x10, Some((OTHER_IOVA, true)))?;
    verify(guarded.virtual_address(), GUARD)?;

    let readonly = page(PATTERN_B)?;
    let readonly_address = readonly.virtual_address();
    controller.map_page(domain_a, OTHER_IOVA, readonly, Permissions::Read)?;
    // A denied write must carry bytes different from the protected contents.
    a.read_dma(IOVA)?;
    a.probe_write(OTHER_IOVA)?;
    fault(controller, sid_a, 0x13, Some((OTHER_IOVA, false)))?;
    verify(readonly_address, PATTERN_B)?;
    a.read_dma(OTHER_IOVA)?;
    a.write_dma(IOVA)?;
    clean(controller)?;
    verify(address_a, PATTERN_B)?; // read-only DMA actually returned its data
    fill(address_a, PATTERN_A);
    a.read_dma(IOVA)?;
    a.probe_write(OTHER_IOVA)?;
    warm_permission_fault(controller, sid_a, OTHER_IOVA, false)?;
    verify(readonly_address, PATTERN_B)?;
    let readonly = controller.unmap_page(domain_a, OTHER_IOVA)?;
    controller.map_page(domain_a, OTHER_IOVA, readonly, Permissions::Write)?;
    a.probe_read(OTHER_IOVA)?;
    fault(controller, sid_a, 0x13, Some((OTHER_IOVA, true)))?;
    // A denied read must not disclose the protected pattern through the device.
    a.write_dma(IOVA)?;
    verify_not_word(address_a, PATTERN_B as u32)?;
    fill(address_a, PATTERN_A);
    // Restore a known buffer after the denied read, then prove write-only works.
    a.read_dma(IOVA)?;
    a.write_dma(OTHER_IOVA)?;
    clean(controller)?;
    verify(readonly_address, PATTERN_A)?;
    // Warm a write-only translation, then attempt a read of new secret bytes.
    fill(readonly_address, PATTERN_B);
    a.probe_read(OTHER_IOVA)?;
    warm_permission_fault(controller, sid_a, OTHER_IOVA, true)?;
    a.write_dma(IOVA)?;
    verify_not_word(address_a, PATTERN_B as u32)?;
    fill(address_a, PATTERN_A);
    a.read_dma(IOVA)?;
    drop(controller.unmap_page(domain_a, OTHER_IOVA)?);
    crate::pr_info!("HypeR test: SMMUv3 unmapped and read/write permission faults passed");

    // Warm translation -> revoke -> denied DMA -> remap to a different PA.
    let old_a = controller.unmap_page(domain_a, IOVA)?;
    fill(old_a.virtual_address(), GUARD);
    a.probe_write(IOVA)?;
    fault(controller, sid_a, 0x10, Some((IOVA, false)))?;
    verify(old_a.virtual_address(), GUARD)?;
    let replacement = page(PATTERN_B)?;
    let replacement_address = replacement.virtual_address();
    controller.map_page(domain_a, IOVA, replacement, Permissions::ReadWrite)?;
    a.write_dma(IOVA)?;
    clean(controller)?;
    verify(replacement_address, PATTERN_A)?;
    verify(old_a.virtual_address(), GUARD)?;

    controller.detach(sid_a)?;
    a.probe_write(IOVA)?;
    fault(controller, sid_a, 4, None)?;
    controller.attach(sid_a, domain_b)?;
    a.write_dma(IOVA)?;
    clean(controller)?;
    verify(address_b, PATTERN_A)?;
    crate::pr_info!("HypeR test: SMMUv3 revoke/remap and stream reassignment passed");

    // Exercise pointer wrap independently of table/TLB warming.
    for _ in 0..600 {
        controller.synchronize()?;
    }
    for _ in 0..260 {
        b.probe_write(OTHER_IOVA)?;
        fault(controller, sid_b, 0x10, Some((OTHER_IOVA, false)))?;
    }
    crate::pr_info!("HypeR test: SMMUv3 command and event queue wrap passed");

    controller.detach(sid_a)?;
    controller.detach(sid_b)?;
    drop(controller.unmap_page(domain_a, IOVA)?);
    drop(controller.unmap_page(domain_b, IOVA)?);
    controller.destroy_domain(domain_a)?;
    controller.destroy_domain(domain_b)?;
    let reused = controller.create_domain()?;
    if reused == domain_a || controller.attach(sid_a, domain_a) != Err(Error::InvalidDomain) {
        return Err(Error::Corrupt);
    }
    controller.map_page(reused, IOVA, old_a, Permissions::ReadWrite)?;
    controller.attach(sid_a, reused)?;
    a.read_dma(IOVA)?;
    clean(controller)?;
    controller.detach(sid_a)?;
    drop(controller.unmap_page(reused, IOVA)?);
    controller.destroy_domain(reused)?;
    crate::pr_info!("HypeR test: SMMUv3 DMA isolation acceptance passed");
    Ok(())
}

fn word(bytes: &[u8]) -> Result<u32, Error> {
    bytes
        .try_into()
        .map(u32::from_be_bytes)
        .map_err(|_| Error::Unsupported)
}
fn page(pattern: u64) -> Result<CoherentMemory, Error> {
    let memory = E::allocate(0)?;
    fill(memory.virtual_address(), pattern);
    Ok(memory)
}
fn fill(address: usize, pattern: u64) {
    for index in 0..512 {
        // SAFETY: Each caller owns a page (directly or via the live controller),
        // and waits for EDU DMA completion before touching its data.
        unsafe { core::ptr::write_volatile((address as *mut u64).add(index), pattern) };
    }
    E::synchronize();
}
fn verify(address: usize, pattern: u64) -> Result<(), Error> {
    E::synchronize();
    for index in 0..512 {
        // SAFETY: Same live-page and completed-DMA contract as fill.
        if unsafe { core::ptr::read_volatile((address as *const u64).add(index)) } != pattern {
            return Err(Error::Corrupt);
        }
    }
    Ok(())
}
fn clean(controller: &mut Controller<E>) -> Result<(), Error> {
    controller.synchronize()?;
    if let Some(event) = controller.next_event()? {
        crate::pr_err!("HypeR test: unexpected SMMUv3 event {event:?}");
        return Err(Error::Corrupt);
    }
    Ok(())
}

fn verify_not_word(address: usize, forbidden: u32) -> Result<(), Error> {
    E::synchronize();
    // SAFETY: Live page retained by the controller; EDU has completed its DMA.
    if unsafe { core::ptr::read_volatile(address as *const u32) } == forbidden {
        return Err(Error::Corrupt);
    }
    Ok(())
}

fn warm_permission_fault(
    controller: &mut Controller<E>,
    stream: u32,
    address: u64,
    read: bool,
) -> Result<(), Error> {
    controller.synchronize()?;
    if let Some(event) = controller.next_event()? {
        if event.kind() != 0x13
            || event.stream() != stream
            || event.address() != address
            || event.read() != read
            || event.words[1] & (1 << 39) == 0
        {
            return Err(Error::Corrupt);
        }
    } else {
        // QEMU 11.1.2: smmu_translate's cached write-permission fault is
        // attributed to stage 1; cached read rejection has no fault record.
        // Do not change the architectural STE to hide this emulator limitation.
        // Cold faults above remain strict, and callers verify data protection.
        crate::pr_warn!(
            "HypeR test: QEMU limitation: warm SMMUv3 permission fault has no event (read={read})"
        );
    }
    clean(controller)
}
fn fault(
    controller: &mut Controller<E>,
    stream: u32,
    kind: u8,
    transaction: Option<(u64, bool)>,
) -> Result<(), Error> {
    controller.synchronize()?; // makes terminated fault records observable
    let event = controller.next_event()?.ok_or(Error::Corrupt)?;
    if event.kind() != kind
        || event.stream() != stream
        || transaction.is_some_and(|(address, read)| {
            event.address() != address || event.read() != read || event.words[1] & (1 << 39) == 0
        })
    {
        crate::pr_err!(
            "HypeR test: wrong SMMUv3 fault {event:?}, expected SID {stream} kind {kind}"
        );
        return Err(Error::Corrupt);
    }
    clean(controller)
}
