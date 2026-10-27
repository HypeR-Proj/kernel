// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Single supervisor-context PLIC. Source zero and machine context zero are reserved.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidAccess,
    InvalidSource,
}

pub struct VirtualPlic {
    priority: [u8; 32],
    enabled: u32,
    pending: u32,
    in_service: u32,
    asserted: u32,
    threshold: u8,
}

impl Default for VirtualPlic {
    fn default() -> Self {
        Self::new()
    }
}
impl VirtualPlic {
    pub const fn new() -> Self {
        Self {
            priority: [0; 32],
            enabled: 0,
            pending: 0,
            in_service: 0,
            asserted: 0,
            threshold: 0,
        }
    }
    pub fn set_level(&mut self, source: u32, asserted: bool) -> Result<(), Error> {
        if !(1..32).contains(&source) {
            return Err(Error::InvalidSource);
        }
        let bit = 1u32 << source;
        if asserted {
            self.asserted |= bit;
            if self.in_service & bit == 0 {
                self.pending |= bit;
            }
        } else {
            self.asserted &= !bit;
        }
        Ok(())
    }
    fn best(&self) -> u32 {
        let eligible = self.pending & self.enabled;
        let mut best = 0;
        for id in 1..32 {
            if eligible & (1 << id) != 0 && self.priority[id] > self.priority[best] {
                best = id;
            }
        }
        best as u32
    }
    pub fn interrupt_asserted(&self) -> bool {
        let best = self.best();
        best != 0 && self.priority[best as usize] > self.threshold
    }
    pub fn read(&mut self, offset: u64) -> Result<u32, Error> {
        if offset & 3 != 0 {
            return Err(Error::InvalidAccess);
        }
        match offset {
            0..=0x7c => Ok(u32::from(self.priority[offset as usize / 4])),
            0x1000 => Ok(self.pending),
            0x2080 => Ok(self.enabled),
            0x201000 => Ok(u32::from(self.threshold)),
            0x201004 => {
                // Claim is independent of the notification threshold.
                let id = self.best();
                if id != 0 {
                    self.pending &= !(1 << id);
                    self.in_service |= 1 << id;
                }
                Ok(id)
            }
            // Unimplemented sources and machine context are hardwired zero.
            offset if offset <= 0x3ffffc => Ok(0),
            _ => Err(Error::InvalidAccess),
        }
    }
    pub fn write(&mut self, offset: u64, value: u32) -> Result<(), Error> {
        if offset & 3 != 0 {
            return Err(Error::InvalidAccess);
        }
        match offset {
            4..=0x7c => self.priority[offset as usize / 4] = (value & 7) as u8,
            0x2080 => self.enabled = value & !1,
            0x201000 => self.threshold = (value & 7) as u8,
            0x201004 if (1..32).contains(&value) => {
                let bit = 1 << value;
                // Ignore completions not enabled in this context or not claimed.
                if self.enabled & self.in_service & bit != 0 {
                    self.in_service &= !bit;
                    if self.asserted & bit != 0 {
                        self.pending |= bit;
                    }
                }
            }
            offset if offset <= 0x3ffffc => {}
            _ => return Err(Error::InvalidAccess),
        }
        Ok(())
    }
}
