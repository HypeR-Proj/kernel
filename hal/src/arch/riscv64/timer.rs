// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;

use hyper::hal::timer::{DeadlineTimer, MonotonicCounter};
use hyper::sync::atomic::{AtomicU64, Ordering};

use super::{registers, sbi};

static FREQUENCY: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidFrequency,
    Firmware(sbi::Error),
}

pub struct RiscvTimeCounter;
pub struct SupervisorTimer;

pub fn set_frequency(frequency: u64) -> Result<(), Error> {
    if frequency == 0 {
        return Err(Error::InvalidFrequency);
    }
    match FREQUENCY.compare_exchange(0, frequency, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Ok(()),
        Err(current) if current == frequency => Ok(()),
        Err(_) => Err(Error::InvalidFrequency),
    }
}

impl MonotonicCounter for RiscvTimeCounter {
    type Error = Error;

    fn frequency_hz() -> Result<u64, Self::Error> {
        match FREQUENCY.load(Ordering::Acquire) {
            0 => Err(Error::InvalidFrequency),
            value => Ok(value),
        }
    }

    fn read() -> u64 {
        let value: u64;
        // SAFETY: TIME is a read-only counter available in HS mode.
        unsafe { asm!("csrr {value}, time", value = out(reg) value, options(nomem, nostack)) };
        value
    }
}

impl DeadlineTimer for SupervisorTimer {
    type Error = Error;

    fn set_deadline(deadline: u64) -> Result<(), Self::Error> {
        sbi::set_timer(deadline).map_err(Error::Firmware)?;
        // SAFETY: SIE.STIE is writable in HS mode after the deadline is programmed.
        unsafe {
            asm!(
                "csrs sie, {mask}",
                mask = in(reg) registers::SIE_STIE as usize,
                options(nostack)
            )
        };
        Ok(())
    }

    fn mask() {
        // SAFETY: Clearing SIE.STIE is valid in HS mode.
        unsafe {
            asm!(
                "csrc sie, {mask}",
                mask = in(reg) registers::SIE_STIE as usize,
                options(nostack)
            )
        };
    }

    fn disable() {
        Self::mask();
        let _ = sbi::set_timer(u64::MAX);
    }
}
