// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fixed-capacity formatting retained across nonblocking output attempts.

use core::fmt;

use {
    hyper::sync::atomic::AtomicU8, hyper::sync::atomic::AtomicUsize, hyper::sync::atomic::Ordering,
};

const NORMAL_IDLE: u8 = 0;
const NORMAL_ACTIVE: u8 = 1;
const STOP_REQUESTED: u8 = 2;
const QUIESCED: u8 = 3;
const EMERGENCY: u8 = 4;
const NO_ACTIVE_CPU: usize = usize::MAX;

/// Result of attempting to acquire normal-runtime UART ownership.
pub enum RuntimeByteAccess<'a> {
    Acquired(RuntimeBytePermit<'a>),
    Busy,
    Retired,
}

/// Result of a bounded transition to emergency UART ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmergencyQuiescence {
    Quiescent,
    LocalOwnerAbandoned,
    RemoteOwnerTimedOut,
}

/// One-way ownership gate between the normal console worker and fatal output.
///
/// Ownership moves monotonically through `NORMAL_IDLE`, `NORMAL_ACTIVE`,
/// `STOP_REQUESTED`, `QUIESCED`, and `EMERGENCY`. A normal acquisition
/// therefore linearizes either before a stop request, in which case it must
/// acknowledge that request while releasing its byte permit, or afterwards,
/// in which case it is rejected. Emergency retirement never takes a lock and
/// waits for at most the caller-supplied number of observations.
pub struct EmergencyWriteGate {
    state: AtomicU8,
    active_cpu: AtomicUsize,
}

impl EmergencyWriteGate {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(NORMAL_IDLE),
            active_cpu: AtomicUsize::new(NO_ACTIVE_CPU),
        }
    }

    /// Attempts to own exactly one normal-runtime UART byte transaction.
    pub fn try_begin_normal_byte(&self, cpu: usize) -> RuntimeByteAccess<'_> {
        match self.state.load(Ordering::Acquire) {
            NORMAL_IDLE => {}
            NORMAL_ACTIVE => return RuntimeByteAccess::Busy,
            STOP_REQUESTED | QUIESCED | EMERGENCY => return RuntimeByteAccess::Retired,
            _ => return RuntimeByteAccess::Retired,
        }
        // Claim the identity slot before publishing NORMAL_ACTIVE. A second
        // normal caller cannot overwrite the true owner while deciding that
        // the byte gate is busy.
        if self
            .active_cpu
            .compare_exchange(NO_ACTIVE_CPU, cpu, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return if self.state.load(Ordering::Acquire) >= STOP_REQUESTED {
                RuntimeByteAccess::Retired
            } else {
                RuntimeByteAccess::Busy
            };
        }
        // The CPU identity precedes the release half of this state CAS. An
        // emergency observer which acquires NORMAL_ACTIVE therefore sees the
        // matching identity rather than a previous transaction's value.
        match self.state.compare_exchange(
            NORMAL_IDLE,
            NORMAL_ACTIVE,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => RuntimeByteAccess::Acquired(RuntimeBytePermit { gate: self }),
            Err(state) => {
                let _ = self.active_cpu.compare_exchange(
                    cpu,
                    NO_ACTIVE_CPU,
                    Ordering::Release,
                    Ordering::Relaxed,
                );
                if state >= STOP_REQUESTED {
                    RuntimeByteAccess::Retired
                } else {
                    RuntimeByteAccess::Busy
                }
            }
        }
    }

    /// Permanently retires normal writes and waits a bounded number of polls.
    pub fn retire_normal_writer(
        &self,
        current_cpu: usize,
        poll_limit: usize,
    ) -> EmergencyQuiescence {
        let mut observed = self.state.load(Ordering::Acquire);
        loop {
            match observed {
                NORMAL_IDLE => match self.state.compare_exchange_weak(
                    NORMAL_IDLE,
                    QUIESCED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return self.activate_emergency(EmergencyQuiescence::Quiescent),
                    Err(current) => observed = current,
                },
                NORMAL_ACTIVE => match self.state.compare_exchange_weak(
                    NORMAL_ACTIVE,
                    STOP_REQUESTED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(current) => observed = current,
                },
                STOP_REQUESTED => break,
                QUIESCED => {
                    return self.activate_emergency(EmergencyQuiescence::Quiescent);
                }
                EMERGENCY => return EmergencyQuiescence::Quiescent,
                _ => return EmergencyQuiescence::RemoteOwnerTimedOut,
            }
        }

        if self.active_cpu.load(Ordering::Acquire) == current_cpu
            && self
                .state
                .compare_exchange(
                    STOP_REQUESTED,
                    QUIESCED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
        {
            return self.activate_emergency(EmergencyQuiescence::LocalOwnerAbandoned);
        }

        for _ in 0..poll_limit {
            if self.state.load(Ordering::Acquire) == QUIESCED {
                return self.activate_emergency(EmergencyQuiescence::Quiescent);
            }
            core::hint::spin_loop();
        }
        // Fail closed: NORMAL_ACTIVE can no longer be acquired, but direct
        // output remains disabled because a remote MMIO transaction may still
        // own the UART.
        EmergencyQuiescence::RemoteOwnerTimedOut
    }

    pub fn is_retired(&self) -> bool {
        self.state.load(Ordering::Acquire) >= STOP_REQUESTED
    }

    pub fn emergency_enabled(&self) -> bool {
        self.state.load(Ordering::Acquire) == EMERGENCY
    }

    fn activate_emergency(&self, outcome: EmergencyQuiescence) -> EmergencyQuiescence {
        let _ =
            self.state
                .compare_exchange(QUIESCED, EMERGENCY, Ordering::AcqRel, Ordering::Acquire);
        outcome
    }
}

impl Default for EmergencyWriteGate {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII ownership of one normal-runtime UART byte transaction.
pub struct RuntimeBytePermit<'a> {
    gate: &'a EmergencyWriteGate,
}

impl Drop for RuntimeBytePermit<'_> {
    fn drop(&mut self) {
        let mut observed = self.gate.state.load(Ordering::Acquire);
        loop {
            let next = match observed {
                NORMAL_ACTIVE => NORMAL_IDLE,
                STOP_REQUESTED => QUIESCED,
                _ => return,
            };
            match self.gate.state.compare_exchange_weak(
                observed,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.gate.active_cpu.store(NO_ACTIVE_CPU, Ordering::Release);
                    return;
                }
                Err(current) => observed = current,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputError {
    Full,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputProgress {
    pub accepted: usize,
    pub complete: bool,
    /// The sink rejected the next byte; false means only the budget expired.
    pub blocked: bool,
}

/// A formatted byte sequence whose accepted prefix survives backpressure.
pub struct OutputBuffer<const CAPACITY: usize> {
    bytes: [u8; CAPACITY],
    length: usize,
    offset: usize,
}

impl<const CAPACITY: usize> OutputBuffer<CAPACITY> {
    pub const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            length: 0,
            offset: 0,
        }
    }

    pub fn clear(&mut self) {
        self.length = 0;
        self.offset = 0;
    }

    pub const fn is_empty(&self) -> bool {
        self.offset == self.length
    }

    pub const fn remaining(&self) -> usize {
        self.length - self.offset
    }

    pub fn push_byte(&mut self, byte: u8) -> Result<(), OutputError> {
        let Some(slot) = self.bytes.get_mut(self.length) else {
            return Err(OutputError::Full);
        };
        *slot = byte;
        self.length += 1;
        Ok(())
    }

    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<(), OutputError> {
        for &byte in bytes {
            self.push_byte(byte)?;
        }
        Ok(())
    }

    /// Attempts at most `limit` bytes, retaining the first rejected byte.
    pub fn try_write(&mut self, limit: usize, mut write: impl FnMut(u8) -> bool) -> OutputProgress {
        let mut accepted = 0;
        let mut blocked = false;
        while accepted < limit && self.offset < self.length {
            if !write(self.bytes[self.offset]) {
                blocked = true;
                break;
            }
            self.offset += 1;
            accepted += 1;
        }
        OutputProgress {
            accepted,
            complete: self.offset == self.length,
            blocked,
        }
    }
}

impl<const CAPACITY: usize> fmt::Write for OutputBuffer<CAPACITY> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.push_bytes(text.as_bytes()).map_err(|_| fmt::Error)
    }
}

impl<const CAPACITY: usize> Default for OutputBuffer<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}
