// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `SMMUv3` non-secure stage-2 driver. No bypass, ATS, PRI, or stall admission.
//!
//! Each controller is exclusively owned. Methods require `&mut self`; callers
//! must not hold IRQ-masked locks across allocation or command completion waits.
//! A published controller is permanent: Drop quarantines all hardware backing.
//! Mapping removal uses TLBI + `CMD_SYNC`, not PCI bus-master-disable as a fence.

mod domain;
mod fault;
pub mod firmware;
mod memory;
mod registers;

use crate::platform::PermanentMmioMapping;
use alloc::vec::Vec;
use core::mem::ManuallyDrop;
pub use domain::{DomainId, Permissions};
pub use fault::{CommandError, Containment, Event, Failure, FaultKind, FaultOutcome};
pub use memory::{DmaBuffer, DmaMemory, Environment};
pub use registers::Capabilities;
use registers::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Unsupported,
    Address,
    Allocation,
    AlreadyEnabled,
    InvalidDomain,
    StreamBusy,
    StreamUnbound,
    StreamQuarantined,
    FaultsPending,
    UnexpectedEvent,
    AlreadyMapped,
    NotMapped,
    DomainBusy,
    Exhausted,
    Corrupt,
    Failed,
    EventOverflow,
    Global(u32),
    Timeout { register: usize, value: u32 },
}

struct Resources<M, B> {
    streams: M,
    commands: M,
    events: M,
    domains: Vec<Option<domain::Domain<M, B>>>,
    bindings: Vec<(u32, DomainId)>,
    quarantined: Vec<u64>,
}

pub struct Controller<E: Environment, B: DmaBuffer = <E as Environment>::Memory> {
    registers: Registers,
    capabilities: Capabilities,
    resources: ManuallyDrop<Resources<E::Memory, B>>,
    published: bool,
    failure: Option<Failure>,
    producer: u32,
    consumer: u32,
    next_serial: u64,
}

impl<E: Environment, B: DmaBuffer> Controller<E, B> {
    /// Acquire a cold SMMU with all upstream bus masters quiescent.
    ///
    /// # Safety
    /// Caller exclusively owns the real SMMU register window and must keep it
    /// inaccessible to untrusted software. Firmware must have stopped upstream
    /// DMA before entry. `dma-coherent` must be established for the table/queue
    /// path, not inferred solely from IDR0. Existing live controllers cannot be
    /// reset through this interface. The register mapping must last until reboot.
    pub unsafe fn initialize(mapping: PermanentMmioMapping) -> Result<Self, Error> {
        let registers = Registers::new(mapping)?;
        if registers.read::<E>(CR0) != 0 || registers.read::<E>(CR0ACK) != 0 {
            return Err(Error::AlreadyEnabled);
        }
        let capabilities = Capabilities::decode(
            registers.read::<E>(IDR0),
            registers.read::<E>(IDR1),
            registers.read::<E>(IDR5),
        )?;
        let mut quarantined = Vec::new();
        let words = (1usize << capabilities.stream_bits).div_ceil(64);
        quarantined
            .try_reserve_exact(words)
            .map_err(|_| Error::Allocation)?;
        quarantined.resize(words, 0);
        let resources = Resources {
            streams: memory::allocate::<E>(
                (capabilities.stream_bits as usize + 6).saturating_sub(12),
                capabilities.address_bits,
            )?,
            commands: memory::allocate::<E>(0, capabilities.address_bits)?,
            events: memory::allocate::<E>(0, capabilities.address_bits)?,
            domains: Vec::new(),
            bindings: Vec::new(),
            quarantined,
        };
        let mut controller = Self {
            registers,
            capabilities,
            resources: ManuallyDrop::new(resources),
            published: false,
            failure: None,
            producer: 0,
            consumer: 0,
            next_serial: 1,
        };
        if let Err(error) = controller.enable() {
            controller.fail_closed(error);
            return Err(error);
        }
        Ok(controller)
    }

    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn enable(&mut self) -> Result<(), Error> {
        self.registers.wait::<E>(GBPA, UPDATE, 0)?;
        let bypass = self.registers.read::<E>(GBPA);
        self.registers.write::<E>(GBPA, bypass | UPDATE | ABORT);
        self.registers.wait::<E>(GBPA, UPDATE, 0)?;
        // Interrupts remain disabled until the host publishes its IRQ owner.
        self.registers.write::<E>(IRQ_CTRL, 0);
        self.registers.wait::<E>(IRQ_CTRLACK, u32::MAX, 0)?;
        // Firmware may leave MSI targets at UNKNOWN values even with CR0=0.
        // ADDR=0 explicitly selects wired IRQ delivery; these are RES0 when MSI
        // is absent. Change only while the corresponding IRQ enable is clear.
        self.registers.write64::<E>(GERROR_IRQ_CFG0, 0);
        self.registers.write64::<E>(EVENTQ_IRQ_CFG0, 0);
        // Normal WB, Inner Shareable queues and configuration fetches.
        self.registers.write::<E>(
            CR1,
            (3 << 10) | (1 << 8) | (1 << 6) | (3 << 4) | (1 << 2) | 1,
        );
        self.registers.write::<E>(CR2, 1 << 1); // record invalid StreamIDs
        self.check()?;
        E::synchronize();
        // From the first base publication onward, any error retains all backing.
        self.published = true;
        self.registers
            .write64::<E>(STRTAB_BASE, self.resources.streams.physical() | (1 << 62));
        self.registers
            .write::<E>(STRTAB_BASE_CFG, self.capabilities.stream_bits.into());
        self.registers.write64::<E>(
            CMDQ_BASE,
            self.resources.commands.physical()
                | u64::from(self.capabilities.command_bits)
                | (1 << 62),
        );
        self.registers.write::<E>(CMDQ_CONS, 0);
        self.registers.write::<E>(CMDQ_PROD, 0);
        self.registers.write64::<E>(
            EVENTQ_BASE,
            self.resources.events.physical() | u64::from(self.capabilities.event_bits) | (1 << 62),
        );
        self.registers.write::<E>(EVENTQ_CONS, 0);
        self.registers.write::<E>(EVENTQ_PROD, 0);
        self.set_control(CMDQEN)?;
        self.command(CFGI_ALL)?;
        self.command(TLBI_NSNH_ALL)?;
        self.synchronize()?;
        self.set_control(CMDQEN | EVENTQEN | SMMUEN)
    }

    fn set_control(&mut self, value: u32) -> Result<(), Error> {
        // ATS fast mode would bypass all checks for Translated requests. Safe
        // mode enforces EATS=0 even against an endpoint emitting such requests.
        let value = value | self.capabilities.ats_check;
        self.registers.write::<E>(CR0, value);
        let result = self.registers.wait::<E>(CR0ACK, u32::MAX, value);
        if let Err(error) = result {
            self.fail_closed(error);
        }
        result
    }

    fn check(&mut self) -> Result<(), Error> {
        if self.failure.is_some() {
            return Err(Error::Failed);
        }
        let errors = self.registers.read::<E>(GERROR) ^ self.registers.read::<E>(GERRORN);
        if errors != 0 {
            let error = Error::Global(errors);
            self.fail_closed(error);
            return Err(error);
        }
        // Overflow is terminal evidence loss even if no worker has polled the
        // queue yet. Do not admit mappings or return retired memory past it.
        // Before publication firmware's queue pointers are not ours to inspect.
        if self.published {
            self.pending_events()?;
        }
        Ok(())
    }

    fn command(&mut self, command: [u64; 2]) -> Result<(), Error> {
        self.check()?;
        let bits = self.capabilities.command_bits;
        let index_mask = (1 << bits) - 1;
        let pointer_mask = (1 << (bits + 1)) - 1;
        let start = E::now_microseconds();
        while self.producer ^ (self.registers.read::<E>(CMDQ_CONS) & pointer_mask) == 1 << bits {
            self.check()?;
            if E::now_microseconds().wrapping_sub(start) >= TIMEOUT_US {
                return self.timeout(CMDQ_CONS);
            }
            core::hint::spin_loop();
        }
        let word = (self.producer & index_mask) as usize * 2;
        memory::write(&self.resources.commands, word, command[0]);
        memory::write(&self.resources.commands, word + 1, command[1]);
        E::synchronize();
        self.producer = (self.producer + 1) & pointer_mask;
        self.registers.write::<E>(CMDQ_PROD, self.producer);
        Ok(())
    }

    fn timeout(&mut self, register: usize) -> Result<(), Error> {
        let error = Error::Timeout {
            register,
            value: self.registers.read::<E>(register),
        };
        self.fail_closed(error);
        Err(error)
    }

    /// Completion of prior commands, affected DMA, and terminated fault records
    /// as specified by IHI 0070 §4.7.3. This does not stop a device issuing work.
    pub fn synchronize(&mut self) -> Result<(), Error> {
        self.command(SYNC)?;
        let mask = (1 << (self.capabilities.command_bits + 1)) - 1;
        let start = E::now_microseconds();
        loop {
            self.check()?;
            if self.registers.read::<E>(CMDQ_CONS) & mask == self.producer {
                break;
            }
            if E::now_microseconds().wrapping_sub(start) >= TIMEOUT_US {
                return self.timeout(CMDQ_CONS);
            }
            core::hint::spin_loop();
        }
        E::synchronize();
        self.check()
    }

    fn pending_events(&mut self) -> Result<u32, Error> {
        let producer = self.registers.read::<E>(EVENTQ_PROD);
        // Never acknowledge and silently lose evidence of an overflow.
        if (producer ^ self.consumer) & (1 << 31) != 0 {
            self.fail_closed(Error::EventOverflow);
            return Err(Error::EventOverflow);
        }
        let bits = self.capabilities.event_bits;
        let mask = (1 << (bits + 1)) - 1;
        let pending = ((producer & mask).wrapping_sub(self.consumer)) & mask;
        if pending > 1 << bits {
            self.fail_closed(Error::Corrupt);
            return Err(Error::Corrupt);
        }
        Ok(pending)
    }

    /// Consumes one hardware event without applying fault containment itself.
    ///
    /// Handle returned evidence with [`Self::quarantine_fault`] before changing
    /// stream ownership: events identify a stream, not its assignment generation.
    /// Queue overflow closes admission instead of acknowledging lost evidence.
    pub fn next_event(&mut self) -> Result<Option<Event>, Error> {
        self.check()?;
        if self.pending_events()? == 0 {
            return Ok(None);
        }
        let bits = self.capabilities.event_bits;
        let mask = (1 << (bits + 1)) - 1;
        E::synchronize();
        let index = (self.consumer & ((1 << bits) - 1)) as usize * 4;
        let event = Event {
            words: core::array::from_fn(|word| memory::read(&self.resources.events, index + word)),
        };
        E::synchronize();
        self.consumer = (self.consumer + 1) & mask;
        self.registers.write::<E>(EVENTQ_CONS, self.consumer);
        Ok(Some(event))
    }

    pub fn create_domain(&mut self) -> Result<DomainId, Error> {
        self.check()?;
        let serial = self.next_serial;
        self.next_serial = serial.checked_add(1).ok_or(Error::Exhausted)?;
        let slot = self
            .resources
            .domains
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.resources.domains.len());
        if slot >= usize::from(self.capabilities.vmid_limit) {
            return Err(Error::Exhausted);
        }
        self.resources
            .domains
            .try_reserve(1)
            .map_err(|_| Error::Allocation)?;
        let id = DomainId {
            controller: self.registers.identity(),
            serial,
            vmid: (slot + 1) as u16,
        };
        let domain = domain::Domain::new::<E>(id, self.capabilities.address_bits)?;
        if slot == self.resources.domains.len() {
            self.resources.domains.push(Some(domain));
        } else {
            self.resources.domains[slot] = Some(domain);
        }
        Ok(id)
    }

    fn domain_index(&self, id: DomainId) -> Result<usize, Error> {
        self.resources
            .domains
            .iter()
            .position(|domain| domain.as_ref().is_some_and(|domain| domain.id == id))
            .ok_or(Error::InvalidDomain)
    }

    fn invalidate_domain(&mut self, id: DomainId) -> Result<(), Error> {
        self.command([0x28 | (u64::from(id.vmid) << 32), 0])?;
        self.synchronize()
    }

    /// Takes one page allocation and publishes an IOVA mapping with the given rights.
    ///
    /// Success includes domain invalidation completion. Before publication an
    /// error drops the supplied allocation; after publication a command failure
    /// retains it in the controller. An error therefore does not imply rollback
    /// of the mapping or permission to reuse its backing.
    pub fn map_page(
        &mut self,
        id: DomainId,
        iova: u64,
        memory: B,
        permissions: Permissions,
    ) -> Result<(), Error> {
        if memory.length() != 4096 {
            return Err(Error::Address);
        }
        self.map_buffer(id, iova, memory, permissions)
    }

    /// Publishes one retained buffer, possibly noncontiguous in physical RAM.
    /// Validation/allocation precede leaf publication; one TLBI completion
    /// covers the whole buffer. Errors after publication retain its owner.
    pub fn map_buffer(
        &mut self,
        id: DomainId,
        iova: u64,
        memory: B,
        permissions: Permissions,
    ) -> Result<(), Error> {
        self.check()?;
        let index = self.domain_index(id)?;
        self.resources.domains[index]
            .as_mut()
            .ok_or(Error::InvalidDomain)?
            .map::<E>(iova, memory, permissions, self.capabilities.address_bits)?;
        self.invalidate_domain(id)
    }

    /// Remove a mapping and return its allocation only after successful TLBI
    /// completion. On failure the allocation remains pinned in this controller.
    /// Reuse of the IOVA for a new owner additionally requires device quiescence.
    pub fn unmap_page(&mut self, id: DomainId, iova: u64) -> Result<B, Error> {
        self.check()?;
        let index = self.domain_index(id)?;
        let mapping = self.resources.domains[index]
            .as_ref()
            .ok_or(Error::InvalidDomain)?;
        let position = mapping
            .mappings
            .binary_search_by_key(&iova, |map| map.iova)
            .map_err(|_| Error::NotMapped)?;
        if mapping.mappings[position].memory.length() != 4096 {
            return Err(Error::Address);
        }
        self.unmap_buffer(id, iova)
    }

    /// Revokes the complete buffer at its original IOVA, returning its lease
    /// only after translation invalidation completes. Partial removal is denied.
    pub fn unmap_buffer(&mut self, id: DomainId, iova: u64) -> Result<B, Error> {
        self.check()?;
        let index = self.domain_index(id)?;
        let mapping = self.resources.domains[index]
            .as_mut()
            .ok_or(Error::InvalidDomain)?
            .invalidate::<E>(iova, self.capabilities.address_bits)?;
        if let Err(error) = self.resources.domains[index]
            .as_mut()
            .ok_or(Error::InvalidDomain)?
            .detach_empty_tables()
        {
            // Partial detachment must not permit a new parent to be published
            // before invalidation. Quarantine every owner on corruption.
            self.fail_closed(error);
            return Err(error);
        }
        // S12_VMALL invalidates intermediate walk caches as well as leaves.
        // CMD_SYNC completes those walks before table or payload pages reuse.
        self.invalidate_domain(id)?;
        Ok(self.resources.domains[index]
            .as_mut()
            .ok_or(Error::InvalidDomain)?
            .finish_unmap(mapping))
    }

    /// Test-only removal of one page from a live production mapping. Both the
    /// original mapping and the returned owner retain backing. Uses the same
    /// domain invalidation and `CMD_SYNC` as ordinary mapping revocation.
    #[cfg(all(feature = "kernel-test", feature = "io-dma-revoke-test"))]
    pub fn revoke_test_page(&mut self, id: DomainId, iova: u64) -> Result<B, Error>
    where
        B: Clone,
    {
        self.check()?;
        let index = self.domain_index(id)?;
        let retained = self.resources.domains[index]
            .as_mut()
            .ok_or(Error::InvalidDomain)?
            .revoke_test_page::<E>(iova, self.capabilities.address_bits)?;
        self.invalidate_domain(id)?;
        Ok(retained)
    }

    /// The caller must quiesce the device before assignment and handle every
    /// consumed event before changing bindings. Events identify only a SID,
    /// not its assignment generation; pending old evidence blocks admission.
    pub fn attach(&mut self, stream: u32, id: DomainId) -> Result<(), Error> {
        self.check()?;
        let domain = self.domain_index(id)?;
        if self.stream_quarantined(stream) {
            return Err(Error::StreamQuarantined);
        }
        if u64::from(stream) >= (1 << self.capabilities.stream_bits) {
            return Err(Error::Address);
        }
        if self
            .resources
            .bindings
            .iter()
            .any(|binding| binding.0 == stream)
        {
            return Err(Error::StreamBusy);
        }
        if self.pending_events()? != 0 {
            return Err(Error::FaultsPending);
        }
        self.resources
            .bindings
            .try_reserve(1)
            .map_err(|_| Error::Allocation)?;
        let root = self.resources.domains[domain]
            .as_ref()
            .ok_or(Error::InvalidDomain)?
            .root_address();
        let word = stream as usize * 8;
        // IHI 0070 §3.21.3.1: hardware may prefetch an invalid STE's words in
        // any order. Synchronize an invalidation with V still clear, then set V
        // and invalidate again; otherwise old root/VMID fields could be combined
        // with a newly observed valid bit during reassignment.
        memory::write(
            &self.resources.streams,
            word + 1,
            self.capabilities.stream_attributes,
        );
        memory::write(
            &self.resources.streams,
            word + 2,
            u64::from(id.vmid)
                | (25 << 32)
                | (1 << 38)
                | (1 << 40)
                | (1 << 42)
                | (3 << 44)
                | (u64::from(self.capabilities.physical_size) << 48)
                | (1 << 51)
                | (1 << 58),
        ); // AA64, record + terminate stage-2 faults
        memory::write(&self.resources.streams, word + 3, root);
        self.resources.bindings.push((stream, id));
        memory::write(&self.resources.streams, word, 6 << 1);
        self.command([3 | (u64::from(stream) << 32), 1])?;
        self.synchronize()?;
        E::synchronize();
        memory::write(&self.resources.streams, word, 1 | (6 << 1));
        self.command([3 | (u64::from(stream) << 32), 1])?;
        self.synchronize()
    }

    /// Denies a stream and retires its cached configuration and translations.
    ///
    /// Remove the software binding only after both completion fences succeed;
    /// failure retains the binding and its domain backing. This does not reset
    /// the device or free the domain's mapped pages.
    pub fn detach(&mut self, stream: u32) -> Result<(), Error> {
        self.check()?;
        if self.stream_quarantined(stream) {
            return Err(Error::StreamQuarantined);
        }
        let index = self
            .resources
            .bindings
            .iter()
            .position(|binding| binding.0 == stream)
            .ok_or(Error::StreamUnbound)?;
        let id = self.resources.bindings[index].1;
        memory::write(&self.resources.streams, stream as usize * 8, 0);
        self.command([3 | (u64::from(stream) << 32), 1])?;
        // Stop old configuration walks before TLB invalidation so they cannot
        // refill translations using the retired STE after its TLB fence.
        self.synchronize()?;
        self.invalidate_domain(id)?;
        self.resources.bindings.swap_remove(index);
        Ok(())
    }

    pub fn destroy_domain(&mut self, id: DomainId) -> Result<(), Error> {
        self.check()?;
        let index = self.domain_index(id)?;
        if self
            .resources
            .bindings
            .iter()
            .any(|binding| binding.1 == id)
            || !self.resources.domains[index]
                .as_ref()
                .ok_or(Error::InvalidDomain)?
                .mappings
                .is_empty()
        {
            return Err(Error::DomainBusy);
        }
        self.invalidate_domain(id)?;
        self.resources.domains[index] = None;
        Ok(())
    }
}

impl<E: Environment, B: DmaBuffer> Drop for Controller<E, B> {
    fn drop(&mut self) {
        if !self.published {
            // SAFETY: No physical base was published; hardware cannot reference
            // these allocations. A live or failed controller instead quarantines
            // every table, queue and still-mapped page until reboot.
            unsafe { ManuallyDrop::drop(&mut self.resources) };
        }
    }
}
