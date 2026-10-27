// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fault evidence, stream quarantine and irreversible controller containment.

use super::{Controller, DomainId, Environment, Error, memory, registers::*};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Event {
    pub words: [u64; 4],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultKind {
    UnsupportedTransaction,
    InvalidStream,
    ConfigurationFetch,
    InvalidConfiguration,
    StreamDisabled,
    ForbiddenTranslation,
    InvalidAtsRequest,
    TranslationConflict,
    ConfigurationConflict,
    Translation,
    AddressSize,
    AccessFlag,
    Permission,
    TranslationFetch,
    Other(u8),
}

impl Event {
    pub fn kind(self) -> u8 {
        self.words[0] as u8
    }
    pub fn stream(self) -> u32 {
        (self.words[0] >> 32) as u32
    }
    pub fn address(self) -> u64 {
        self.words[2]
    }
    pub fn read(self) -> bool {
        self.words[1] & (1 << 35) != 0
    }
    pub fn stalled(self) -> bool {
        self.words[1] & (1 << 31) != 0
    }
    pub fn fault(self) -> FaultKind {
        match self.kind() {
            1 => FaultKind::UnsupportedTransaction,
            2 => FaultKind::InvalidStream,
            3 => FaultKind::ConfigurationFetch,
            4 => FaultKind::InvalidConfiguration,
            5 => FaultKind::InvalidAtsRequest,
            6 => FaultKind::StreamDisabled,
            7 => FaultKind::ForbiddenTranslation,
            0x0b => FaultKind::TranslationFetch,
            0x10 => FaultKind::Translation,
            0x11 => FaultKind::AddressSize,
            0x12 => FaultKind::AccessFlag,
            0x13 => FaultKind::Permission,
            0x20 => FaultKind::TranslationConflict,
            0x21 => FaultKind::ConfigurationConflict,
            other => FaultKind::Other(other),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandError {
    None,
    Illegal,
    FetchAbort,
    AtcInvalidationTimeout,
    Other(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Containment {
    /// GBPA.ABORT and CR0ACK=0 acknowledged. All published memory stays pinned;
    /// this is not a device reset or a proof of physical DMA retirement.
    AbortAcknowledged,
    /// Hardware did not acknowledge containment. The host must fail-stop.
    Unconfirmed(Error),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Failure {
    pub cause: Error,
    pub global_errors: u32,
    pub command_consumer: u32,
    pub command: [u64; 2],
    pub event_producer: u32,
    pub event_consumer: u32,
    pub containment: Containment,
    pub interrupt_error: Option<Error>,
    pub event: Option<Event>,
}

impl Failure {
    pub fn command_error(self) -> CommandError {
        if self.global_errors & 1 == 0 {
            return CommandError::None;
        }
        match ((self.command_consumer >> 24) & 127) as u8 {
            0 => CommandError::None,
            1 => CommandError::Illegal,
            2 => CommandError::FetchAbort,
            3 => CommandError::AtcInvalidationTimeout,
            value => CommandError::Other(value),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FaultOutcome {
    Quarantined { domain: Option<DomainId> },
    AlreadyQuarantined,
}

impl<E: Environment, B: super::DmaBuffer> Controller<E, B> {
    pub fn failure(&self) -> Option<Failure> {
        self.failure
    }

    /// Permanent admission closure, including when normal command completion
    /// is broken. Never skips a failed TLBI or resumes an uncertain queue.
    /// Repeated calls preserve the first evidence and do not retry/reset hardware.
    pub fn fail_closed(&mut self, cause: Error) {
        if self.failure.is_some() {
            return;
        }
        let global = self.registers.read::<E>(GERROR);
        let acknowledged = self.registers.read::<E>(GERRORN);
        let consumer = self.registers.read::<E>(CMDQ_CONS);
        let word = (consumer & ((1 << self.capabilities.command_bits) - 1)) as usize * 2;
        let mut failure = Failure {
            cause,
            global_errors: global ^ acknowledged,
            command_consumer: consumer,
            command: [
                memory::read(&self.resources.commands, word),
                memory::read(&self.resources.commands, word + 1),
            ],
            event_producer: self.registers.read::<E>(EVENTQ_PROD),
            event_consumer: self.consumer,
            containment: Containment::Unconfirmed(cause),
            interrupt_error: None,
            event: None,
        };
        // Close software admission before attempting any hardware transition.
        self.failure = Some(failure);
        self.registers.write::<E>(IRQ_CTRL, 0);
        failure.interrupt_error = self.registers.wait::<E>(IRQ_CTRLACK, u32::MAX, 0).err();
        failure.containment = match self.abort_controller() {
            Ok(()) => {
                // Acknowledge only the captured errors, and only after CMDQ is
                // disabled: acknowledging CMDQ_ERR while enabled resumes it.
                self.registers.write::<E>(GERRORN, global);
                Containment::AbortAcknowledged
            }
            Err(error) => Containment::Unconfirmed(error),
        };
        self.failure = Some(failure);
    }

    fn abort_controller(&mut self) -> Result<(), Error> {
        // GBPA controls SMMUEN=0 traffic. Never disable translation before its
        // abort policy has completed the architectural UPDATE handshake.
        self.registers.wait::<E>(GBPA, UPDATE, 0)?;
        let policy = self.registers.read::<E>(GBPA);
        self.registers.write::<E>(GBPA, policy | ABORT | UPDATE);
        self.registers.wait::<E>(GBPA, UPDATE, 0)?;
        self.registers.write::<E>(CR0, 0);
        self.registers.wait::<E>(CR0ACK, u32::MAX, 0)
    }

    /// The caller must have published handlers for EVENTQ and GERROR first.
    /// The caller must also drain once after enabling: old conditions need not
    /// generate an edge when `IRQ_CTRL` changes from disabled to enabled.
    pub fn enable_interrupts(&mut self) -> Result<(), Error> {
        self.check()?;
        self.registers.write::<E>(IRQ_CTRL, 5);
        if let Err(error) = self.registers.wait::<E>(IRQ_CTRLACK, u32::MAX, 5) {
            self.fail_closed(error);
            return Err(error);
        }
        self.check()
    }

    pub fn stream_quarantined(&self, stream: u32) -> bool {
        self.resources
            .quarantined
            .get(stream as usize / 64)
            .is_some_and(|word| word & (1 << (stream % 64)) != 0)
    }

    /// Quarantine is irreversible until reboot. Preserve bindings and backing,
    /// so an old device cannot acquire another owner's IOVA or reused VMID.
    /// The first fault on a stream closes it without disturbing other domains.
    pub fn quarantine_fault(&mut self, event: Event) -> Result<FaultOutcome, Error> {
        self.check()?;
        let stream = event.stream();
        if event.stalled()
            || matches!(event.fault(), FaultKind::Other(_))
            || u64::from(stream) >= 1 << self.capabilities.stream_bits
        {
            // No reliable attributable stream (or impossible stall protocol).
            // Never discard unknown evidence and continue granting DMA.
            self.fail_closed(Error::UnexpectedEvent);
            if let Some(failure) = &mut self.failure {
                failure.event = Some(event);
            }
            return Err(Error::UnexpectedEvent);
        }
        if self.stream_quarantined(stream) {
            return Ok(FaultOutcome::AlreadyQuarantined);
        }
        self.resources.quarantined[stream as usize / 64] |= 1 << (stream % 64);
        let domain = self
            .resources
            .bindings
            .iter()
            .find(|binding| binding.0 == stream)
            .map(|binding| binding.1);
        if let Err(error) = self.abort_stream(stream, domain) {
            if let Some(failure) = &mut self.failure {
                failure.event = Some(event);
            }
            return Err(error);
        }
        Ok(FaultOutcome::Quarantined { domain })
    }

    fn abort_stream(&mut self, stream: u32, domain: Option<DomainId>) -> Result<(), Error> {
        // Invalidate the old configuration before replacing it with a valid
        // abort STE. CFG=0 denies without generating further events, bounding
        // fault storms. Completion precedes TLB retirement for any old domain.
        memory::write(&self.resources.streams, stream as usize * 8, 0);
        self.command([3 | (u64::from(stream) << 32), 1])?;
        self.synchronize()?;
        memory::write(&self.resources.streams, stream as usize * 8, 1);
        self.command([3 | (u64::from(stream) << 32), 1])?;
        self.synchronize()?;
        if let Some(id) = domain {
            self.invalidate_domain(id)?;
        }
        Ok(())
    }
}
