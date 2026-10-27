// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native block initiators. Userspace negotiates a backend once; filesystem
//! requests use shared virtio-scsi queues and scheduler notifications directly.

mod request;
pub(crate) mod service;
mod transfer;
mod wire;

use crate::authority::Rights;
use crate::mm::user_space::GuestMemoryBacking;
use crate::vm::io::Notification;
use core::sync::atomic::{AtomicU16, Ordering};
use hyper::mm::FallibleArc;
use hyper::sync::InterruptSpinLock;
use {crate::accounting::CommittedCharge, crate::accounting::ResourceDomain};
use {
    crate::fs::primitives::block::BlockDevice, crate::fs::primitives::block::Error,
    crate::fs::primitives::block::SECTOR_SIZE,
};
use {
    crate::object::KernelObject, crate::object::ObjectKind, crate::object::ObjectRetirement,
    crate::object::SignalMask, crate::object::SignalSource, crate::object::SignalState,
    crate::object::SignalWaitOutcome, crate::object::TransferClass, crate::object::private,
    crate::object::wait_one,
};

struct State {
    busy: bool,
    failed: bool,
    sectors: u64,
    indices: [u16; wire::REQUEST_QUEUES],
    tag: u64,
    readonly: bool,
    capability_alive: bool,
}

pub(crate) struct Device {
    #[cfg(feature = "kernel-trace")]
    trace_id: u64,
    // Backend VM mappings independently retain this hardware lease. Disconnect
    // never recycles an in-flight slot: pages survive until VM/DMA retirement.
    memory: GuestMemoryBacking,
    guest_base: u64,
    notification: Notification,
    domain: ResourceDomain,
    state: InterruptSpinLock<State, crate::hal::irq::LocalMask>,
    available: SignalState,

    _charge: CommittedCharge,
}

impl Device {
    fn update_available(&self, state: &State) {
        let bits = u64::from(!state.busy || state.failed);
        if self
            .available
            .update(
                SignalMask::from_trusted_bits(1),
                SignalMask::from_trusted_bits(bits),
            )
            .is_err()
        {
            crate::diagnostics::crash::fatal(format_args!("block admission signal exhausted"));
        }
    }

    fn acquire(&self, require_active: bool) -> Result<Session<'_>, Error> {
        loop {
            let admitted = self.state.with(|state| {
                if state.failed {
                    return Err(Error::Disconnected);
                }
                if require_active && state.sectors == 0 {
                    return Err(Error::Disconnected);
                }
                if state.busy {
                    return Ok(false);
                }
                state.busy = true;
                self.update_available(state);
                Ok(true)
            })?;
            if admitted {
                return Ok(Session { device: self });
            }
            match wait_one(
                SignalSource::new(&self.available, SignalMask::from_trusted_bits(1)),
                &self.domain,
                1,
                u64::MAX,
                || false,
            )
            .map_err(|_| Error::Exhausted)?
            {
                SignalWaitOutcome::Observed(_) => {}
                _ => return Err(Error::Disconnected),
            }
        }
    }

    fn fail(&self) {
        self.state.with(|state| {
            state.failed = true;
            self.update_available(state);
        });
        self.notification.close_native();
    }

    fn read(&self, offset: u64, output: &mut [u8]) -> Result<(), Error> {
        self.memory
            .read_exposed(offset, output)
            .map_err(|_| Error::Io)
    }
    fn write(&self, offset: u64, input: &[u8]) -> Result<(), Error> {
        self.memory
            .write_exposed(offset, input)
            .map_err(|_| Error::Io)
    }
    fn index_word(&self, offset: u64) -> Result<&AtomicU16, Error> {
        if !offset.is_multiple_of(2) || offset + 2 > wire::MEMORY_BYTES {
            return Err(Error::InvalidRange);
        }
        let physical = self
            .memory
            .physical_page(offset & !4095)
            .map_err(|_| Error::Io)?;
        let address = crate::mm::memory::linear_address(physical.get()).ok_or(Error::Io)?;
        let address = address
            .checked_add((offset & 4095) as usize)
            .ok_or(Error::Io)?;
        // SAFETY: The retained hardware lease pins this initialized, aligned
        // word and its permanent linear mapping. Only the designated producer
        // stores each index, using an aligned 16-bit access. Other guest-owned
        // bytes are accessed through the HAL's external-memory copy contract.
        Ok(unsafe { &*core::ptr::with_exposed_provenance::<AtomicU16>(address) })
    }
    fn u16(&self, offset: u64) -> Result<u16, Error> {
        Ok(u16::from_le(
            self.index_word(offset)?.load(Ordering::Acquire),
        ))
    }

    fn activate(&self, readonly: bool) -> Result<u64, Error> {
        let session = self.acquire(false)?;
        if self.state.with(|state| state.sectors != 0) {
            return Err(Error::InvalidRange);
        }
        self.notification
            .control(1)
            .map_err(|_| Error::Disconnected)?;
        // A newly established SCSI nexus can report UNIT ATTENTION. Retrying a
        // read-only discovery command consumes it without replaying any writes.
        let mut capacity = [0; 32];
        let mut last = Err(Error::Io);
        for _ in 0..4 {
            last = session.command(&wire::capacity_cdb(), None, Some(&mut capacity));
            if last.is_ok() || matches!(last, Err(Error::Disconnected | Error::Corrupt)) {
                break;
            }
        }
        last?;
        let last_sector = u64::from_be_bytes(capacity[..8].try_into().map_err(|_| Error::Corrupt)?);
        let size = u32::from_be_bytes(capacity[8..12].try_into().map_err(|_| Error::Corrupt)?);
        if size != SECTOR_SIZE as u32 {
            self.fail();
            return Err(Error::Unsupported);
        }
        let sectors = last_sector
            .checked_add(1)
            .filter(|s| *s != 0)
            .ok_or(Error::Corrupt)?;
        self.state.with(|state| {
            state.sectors = sectors;
            state.readonly = readonly;
        });
        Ok(sectors)
    }
}

struct Session<'a> {
    device: &'a Device,
}
impl Drop for Session<'_> {
    fn drop(&mut self) {
        self.device.state.with(|state| {
            state.busy = false;
            self.device.update_available(state);
        });
    }
}

pub(crate) struct NativeBlock {
    device: FallibleArc<Device>,
}
impl private::Sealed for NativeBlock {}
impl private::UserExportable for NativeBlock {}
impl KernelObject for NativeBlock {
    const KIND: ObjectKind = ObjectKind::NATIVE_BLOCK;
    const TRANSFER_CLASS: TransferClass = TransferClass::RendezvousOnly;
    const SUPPORTED_RIGHTS: Rights = Rights::READ
        .union(Rights::WRITE)
        .union(Rights::MAP)
        .union(Rights::TRANSFER)
        .union(Rights::INSPECT)
        .union(Rights::WAIT);
    fn signal_source(&self) -> Option<SignalSource<'_>> {
        Some(self.device.notification.closed_signal_source())
    }
    fn on_zero_active_handles(&self, _: &mut ObjectRetirement) {
        self.device
            .state
            .with(|state| state.capability_alive = false);
        self.device.fail();
    }
}

pub(crate) struct BlockAccess {
    device: FallibleArc<Device>,
}
impl BlockDevice for BlockAccess {
    fn is_read_only(&self) -> bool {
        self.device.state.with(|state| state.readonly)
    }
    fn sector_count(&self) -> u64 {
        self.device.state.with(|state| state.sectors)
    }
    fn read_sectors(&mut self, first: u64, output: &mut [u8]) -> Result<(), Error> {
        let session = self.device.acquire(true)?;
        check_range(first, output.len(), self.sector_count())?;
        for (batch, bytes) in output
            .chunks_mut(wire::DATA_BYTES * wire::REQUEST_QUEUES)
            .enumerate()
        {
            let sector =
                first + (batch * wire::DATA_BYTES * wire::REQUEST_QUEUES / SECTOR_SIZE) as u64;
            session.read_batch(sector, bytes)?;
        }
        Ok(())
    }
    fn write_sectors(&mut self, first: u64, input: &[u8]) -> Result<(), Error> {
        self.write_batch(&[crate::fs::primitives::block::WriteRequest {
            first,
            bytes: input,
        }])
    }
    fn write_batch(
        &mut self,
        requests: &[crate::fs::primitives::block::WriteRequest<'_>],
    ) -> Result<(), Error> {
        crate::fs::primitives::block::validate_writes(self.sector_count(), requests)?;
        let session = self.device.acquire(true)?;
        if self.device.state.with(|state| state.readonly) {
            return Err(Error::ReadOnly);
        }
        session.write_batch(requests)
    }
    fn flush(&mut self) -> Result<(), Error> {
        let session = self.device.acquire(true)?;
        let mut cdb = [0; 16];
        cdb[0] = 0x91;
        session.command(&cdb, None, None)
    }
}
fn check_range(first: u64, bytes: usize, sectors: u64) -> Result<(), Error> {
    if !bytes.is_multiple_of(SECTOR_SIZE)
        || first
            .checked_add((bytes / SECTOR_SIZE) as u64)
            .is_none_or(|end| end > sectors)
    {
        return Err(Error::InvalidRange);
    }
    Ok(())
}
