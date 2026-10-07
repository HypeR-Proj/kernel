// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Faults handled by the real IRQ/worker path, without polling `EventQ` in tests.

use super::{E, Error, Fixture, GUARD, IOVA, OTHER_IOVA, PATTERN_A, PATTERN_B, fill, page, verify};
use crate::kernel::device::iommu::runtime;
use hyper::{
    drivers::{
        iommu::smmuv3::{CommandError, Containment, DmaMemory, Environment, Permissions},
        platform::{DriverServices, PlatformDevice},
    },
    sync::InterruptSpinLock,
};

struct Prepared {
    fixture: Fixture,
    registers: usize,
}
static FIXTURE: InterruptSpinLock<Option<Prepared>, crate::hal::irq::LocalMask> =
    InterruptSpinLock::new(None);

pub(crate) fn prepare(
    smmu: &PlatformDevice,
    nodes: &[PlatformDevice],
    services: &dyn DriverServices,
) -> Result<(), Error> {
    let mut fixture = Fixture::prepare(smmu, nodes, services)?;
    // Produce an event before IRQ_CTRL is enabled. The runtime must drain it
    // even though the architecture does not promise a new IRQ for an old event.
    fixture.devices[2].probe_read(OTHER_IOVA)?;
    let prepared = Prepared {
        fixture,
        registers: services
            .map_mmio(smmu.registers()[0])
            .map_err(|_| Error::Address)?
            .virtual_start(),
    };
    FIXTURE.with(|slot| *slot = Some(prepared));
    Ok(())
}

pub(crate) fn run() -> Result<(), Error> {
    let timeout = timeout_test()?;
    let Prepared {
        fixture:
            Fixture {
                devices: [mut a, mut b, mut unbound],
                streams: [sid_a, sid_b, sid_c],
            },
        registers,
    } = FIXTURE.with(Option::take).ok_or(Error::Corrupt)?;
    let owner = runtime::owner().ok_or(Error::Corrupt)?;
    wait(|| {
        Ok(read32(registers + 0x54) == 5
            && owner.with_controller(|controller| controller.stream_quarantined(sid_c)))
    })?;
    crate::pr_info!("HypeR test: SMMUv3 pre-enable event drain passed");
    let (domain_a, domain_b, address_a, address_b) =
        owner.with_controller(|controller| -> Result<_, Error> {
            let domain_a = controller.create_domain()?;
            let domain_b = controller.create_domain()?;
            let a = page(PATTERN_A)?;
            let b = page(PATTERN_B)?;
            let addresses = (a.virtual_address(), b.virtual_address());
            controller.map_page(domain_a, IOVA, a, Permissions::ReadWrite)?;
            controller.map_page(domain_b, IOVA, b, Permissions::ReadWrite)?;
            controller.attach(sid_a, domain_a)?;
            controller.attach(sid_b, domain_b)?;
            Ok((domain_a, domain_b, addresses.0, addresses.1))
        })?;
    let before = runtime::interrupt_count();
    a.read_dma(IOVA)?;
    a.probe_write(OTHER_IOVA)?;
    wait(|| Ok(owner.with_controller(|controller| controller.stream_quarantined(sid_a))))?;
    if runtime::interrupt_count() <= before {
        return Err(Error::Corrupt);
    }
    fill(address_a, GUARD);
    a.write_dma(IOVA)?; // warm, previously valid IOVA must now abort too
    verify(address_a, GUARD)?;
    b.read_dma(IOVA)?;
    fill(address_b, GUARD);
    b.write_dma(IOVA)?;
    verify(address_b, PATTERN_B)?;
    owner.with_controller(|controller| {
        if controller.attach(sid_a, domain_b) != Err(Error::StreamQuarantined)
            || controller.detach(sid_a) != Err(Error::StreamQuarantined)
            || controller.destroy_domain(domain_a) != Err(Error::DomainBusy)
        {
            return Err(Error::Corrupt);
        }
        Ok(())
    })?;
    let guarded = page(GUARD)?;
    unbound.probe_write(guarded.physical())?;
    wait(|| Ok(owner.with_controller(|controller| controller.stream_quarantined(sid_c))))?;
    verify(guarded.virtual_address(), GUARD)?;
    // No events after abort-STE completion, even a full failed 4 KiB transfer.
    let before = runtime::interrupt_count();
    unbound.write_dma(guarded.physical())?;
    verify(guarded.virtual_address(), GUARD)?;
    if runtime::interrupt_count() != before {
        return Err(Error::Corrupt);
    }
    crate::pr_info!(
        "HypeR test: SMMUv3 IRQ worker, per-stream quarantine and fault flood suppression passed"
    );

    b.read_dma(IOVA)?; // establish a live translation before control-plane failure
    let before = runtime::interrupt_count();
    owner.with_controller(|controller| {
        if timeout {
            // A stopped command queue cannot consume SYNC and raises no IRQ.
            // The access boundary must prompt the sleeping worker itself.
            write32(registers + 0x20, read32(registers + 0x20) & !(1 << 3));
            wait(|| Ok(read32(registers + 0x24) & (1 << 3) == 0))?;
            if !matches!(
                controller.synchronize(),
                Err(Error::Timeout { register: 0x9c, .. })
            ) {
                return Err(Error::Corrupt);
            }
        } else {
            inject_illegal_command(registers)?;
        }
        Ok(())
    })?;
    wait(|| Ok(runtime::failure_reported()))?;
    let failure = owner
        .with_controller(|controller| controller.failure())
        .ok_or(Error::Corrupt)?;
    let expected_fault = if timeout {
        runtime::interrupt_count() == before
            && matches!(failure.cause, Error::Timeout { register: 0x9c, .. })
            && failure.command_error() == CommandError::None
    } else {
        runtime::interrupt_count() > before
            && failure.cause == Error::Global(1)
            && failure.command_error() == CommandError::Illegal
            && failure.command[0] == 0xff
    };
    if !expected_fault
        || failure.containment != Containment::AbortAcknowledged
        || read32(registers + 0x24) != 0
        || read32(registers + 0x44) & (1 << 20) == 0
    {
        return Err(Error::Corrupt);
    }
    fill(address_b, GUARD);
    b.write_dma(IOVA)?;
    verify(address_b, GUARD)?;
    b.write_dma(guarded.physical())?; // disabling SMMU must not enable PA bypass
    verify(guarded.virtual_address(), GUARD)?;
    if owner
        .with_controller(|controller| controller.unmap_page(domain_b, IOVA))
        .is_ok()
    {
        return Err(Error::Corrupt);
    }
    if timeout {
        crate::pr_info!("HypeR test: SMMUv3 no-IRQ timeout, worker wake and pinned backing passed");
    } else {
        crate::pr_info!(
            "HypeR test: SMMUv3 global-error IRQ, abort containment and pinned backing passed"
        );
    }
    Ok(())
}

fn timeout_test() -> Result<bool, Error> {
    let physical = crate::kernel::boot::with_boot_state(|state| state.dtb_address);
    let address = crate::kernel::mm::memory::linear_address(physical).ok_or(Error::Address)?;
    let mut chosen = hyper::platform::chosen::Discovery::new();
    // SAFETY: The boot DTB stays reserved in the permanent linear map.
    unsafe { hyper::platform::fdt::discover_with(address, &mut chosen) }
        .map_err(|_| Error::Corrupt)?;
    let chosen = chosen.finish().map_err(|_| Error::Corrupt)?;
    match chosen
        .command_line()
        .and_then(|line| line.value("smmu-failure-test"))
    {
        None | Some("gerror") => Ok(false),
        Some("timeout") => Ok(true),
        Some(_) => Err(Error::Unsupported),
    }
}

fn wait(mut condition: impl FnMut() -> Result<bool, Error>) -> Result<(), Error> {
    let start = E::now_microseconds();
    while !condition()? {
        if E::now_microseconds().wrapping_sub(start) > 5_000_000 {
            return Err(Error::Failed);
        }
        crate::kernel::task::scheduler::cond_resched().map_err(|_| Error::Corrupt)?;
        core::hint::spin_loop();
    }
    Ok(())
}

fn read32(address: usize) -> u32 {
    E::synchronize();
    // SAFETY: Test owns aligned registers in the permanent SMMU mapping.
    let value = unsafe { core::ptr::read_volatile(address as *const u32) };
    E::synchronize();
    u32::from_le(value)
}

fn write32(address: usize, value: u32) {
    E::synchronize();
    // SAFETY: Test holds the controller mutex and uses an aligned SMMU register.
    unsafe { core::ptr::write_volatile(address as *mut u32, value.to_le()) };
    E::synchronize();
}

fn inject_illegal_command(registers: usize) -> Result<(), Error> {
    let base = u64::from(read32(registers + 0x90)) | (u64::from(read32(registers + 0x94)) << 32);
    let bits = base as u32 & 31;
    let producer = read32(registers + 0x98);
    if producer != read32(registers + 0x9c) {
        return Err(Error::Corrupt);
    }
    let queue = crate::kernel::mm::memory::linear_address(base & 0x0000_ffff_ffff_f000)
        .ok_or(Error::Address)?;
    let command = queue + (producer & ((1 << bits) - 1)) as usize * 16;
    // SAFETY: Controller mutex excludes both software consumers. The queue is
    // empty, retained in the permanent linear map, and these two aligned words
    // are its next slot. This test ends in permanent controller quarantine;
    // normal queue operations can never resume with the injected producer.
    unsafe {
        core::ptr::write_volatile(command as *mut u64, 0xffu64.to_le());
        core::ptr::write_volatile((command + 8) as *mut u64, 0);
    }
    E::synchronize();
    // SAFETY: Exclusive test publication to the aligned CMDQ_PROD register.
    unsafe {
        core::ptr::write_volatile(
            (registers + 0x98) as *mut u32,
            ((producer + 1) & ((1 << (bits + 1)) - 1)).to_le(),
        );
    }
    E::synchronize();
    Ok(())
}
