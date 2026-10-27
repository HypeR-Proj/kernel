// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded guest power transactions. External serialization owns all transitions.

pub const MAX_CPUS: usize = 8;
pub const SUCCESS: i64 = 0;
pub const NOT_SUPPORTED: i64 = -1;
pub const INVALID_PARAMETERS: i64 = -2;
pub const DENIED: i64 = -3;
pub const ALREADY_ON: i64 = -4;
pub const ON_PENDING: i64 = -5;
pub const INTERNAL_FAILURE: i64 = -6;
pub const INVALID_ADDRESS: i64 = -9;

/// PSCI results are signed 32-bit values with a zero upper word (DEN 0022C 5.2.2).
pub const fn return_register(result: i64) -> u64 {
    result as i32 as u32 as u64
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    CpuOn,
    CpuOff,
    SystemOff,
    SystemReset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub id: u64,
    pub vcpu: u32,
    pub operation: Operation,
    pub target: u32,
    pub entry: u64,
    pub context: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bootstrap {
    pub entry: u64,
    pub context: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Power {
    Off,
    On,
    Starting(u64),
    Stopping(u64),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Transaction {
    Empty,
    Staged(Request),
    Ready(Request),
    Complete(i64),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Continuation {
    Wait,
    Resume(i64),
    Restart(Bootstrap),
}

pub struct PowerState {
    count: usize,
    next: u64,
    power: [Power; MAX_CPUS],
    requests: [Transaction; MAX_CPUS],
    bootstraps: [Option<Bootstrap>; MAX_CPUS],
}

impl PowerState {
    pub fn new(count: usize) -> Option<Self> {
        (1..=MAX_CPUS).contains(&count).then_some(Self {
            count,
            next: 1,
            power: [Power::Off; MAX_CPUS],
            requests: [Transaction::Empty; MAX_CPUS],
            bootstraps: [None; MAX_CPUS],
        })
    }

    pub fn boot(&mut self) -> Result<(), i64> {
        if self.power[0] != Power::Off {
            return Err(ALREADY_ON);
        }
        self.power[0] = Power::On;
        Ok(())
    }

    pub fn affinity(&self, target: u64, level: u64) -> i64 {
        if level > 3 || target & !0x0000_00ff_00ff_ffff != 0 {
            return INVALID_PARAMETERS;
        }
        let ignored = (1u64 << (8 * level)) - 1;
        if (level == 0 && target >= self.count as u64) || (level != 0 && target & !ignored != 0) {
            return INVALID_PARAMETERS;
        }
        let states = if level == 0 {
            &self.power[target as usize..target as usize + 1]
        } else {
            &self.power[..self.count]
        };
        if states
            .iter()
            .any(|s| matches!(s, Power::On | Power::Stopping(_)))
        {
            0
        } else if states.iter().any(|s| matches!(s, Power::Starting(_))) {
            2
        } else {
            1
        }
    }

    /// Reserves the target before the caller leaves hardware. Not yet observable
    /// by the runtime; publication requires the stopped runner's proof.
    pub fn stage(
        &mut self,
        source: u32,
        operation: Operation,
        arguments: [u64; 3],
        ram: core::ops::Range<u64>,
    ) -> Result<(), i64> {
        let cpu = source as usize;
        if cpu >= self.count
            || self.power[cpu] != Power::On
            || self.requests[cpu] != Transaction::Empty
        {
            return Err(DENIED);
        }
        let (target, entry, context) = if operation == Operation::CpuOn {
            let [target, entry, context] = arguments;
            if target >= self.count as u64 {
                return Err(INVALID_PARAMETERS);
            }
            if !entry.is_multiple_of(4) || !ram.contains(&entry) {
                return Err(INVALID_ADDRESS);
            }
            match self.power[target as usize] {
                Power::On | Power::Stopping(_) => return Err(ALREADY_ON),
                Power::Starting(_) => return Err(ON_PENDING),
                Power::Off => {}
            }
            (target as u32, entry, context)
        } else {
            (source, 0, 0)
        };
        let id = self.next;
        self.next = id.checked_add(1).ok_or(INTERNAL_FAILURE)?;
        let request = Request {
            id,
            vcpu: source,
            operation,
            target,
            entry,
            context,
        };
        if operation == Operation::CpuOn {
            self.power[target as usize] = Power::Starting(id);
        }
        if operation == Operation::CpuOff {
            self.power[cpu] = Power::Stopping(id);
        }
        self.requests[cpu] = Transaction::Staged(request);
        Ok(())
    }

    pub fn publish(&mut self, source: u32) -> Result<bool, i64> {
        if source as usize >= self.count {
            return Err(INVALID_PARAMETERS);
        }
        let slot = self
            .requests
            .get_mut(source as usize)
            .ok_or(INVALID_PARAMETERS)?;
        if let Transaction::Staged(request) = *slot {
            *slot = Transaction::Ready(request);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn pending(&self) -> Option<Request> {
        self.requests[..self.count]
            .iter()
            .filter_map(|slot| {
                if let Transaction::Ready(request) = slot {
                    Some(*request)
                } else {
                    None
                }
            })
            .min_by_key(|request| request.id)
    }

    pub fn request(&self, id: u64) -> Option<Request> {
        self.requests[..self.count].iter().find_map(|slot| {
            if let Transaction::Ready(request) = slot {
                (request.id == id).then_some(*request)
            } else {
                None
            }
        })
    }

    /// Caller has prepared every fallible resource before accepting. A power-on
    /// acceptance publishes bootstrap ownership before any scheduler wakeup.
    pub fn complete(&mut self, id: u64, accept: bool) -> Result<Request, i64> {
        let request = self.request(id).ok_or(INVALID_PARAMETERS)?;
        let source = request.vcpu as usize;
        match request.operation {
            Operation::CpuOn => {
                let target = request.target as usize;
                if self.power[target] != Power::Starting(id) {
                    return Err(INTERNAL_FAILURE);
                }
                if accept {
                    self.bootstraps[target] = Some(Bootstrap {
                        entry: request.entry,
                        context: request.context,
                    });
                    self.power[target] = Power::On;
                } else {
                    self.power[target] = Power::Off;
                }
                self.requests[source] =
                    Transaction::Complete(if accept { SUCCESS } else { INTERNAL_FAILURE });
            }
            Operation::CpuOff if accept => {
                self.power[source] = Power::Off;
                self.requests[source] = Transaction::Empty;
            }
            Operation::CpuOff => {
                self.power[source] = Power::On;
                self.requests[source] = Transaction::Complete(DENIED);
            }
            Operation::SystemOff | Operation::SystemReset => {
                // Neither success nor rejection may resume a PSCI system call.
                // Runtime must stop or replace the VM through its lifecycle API.
                return Err(INVALID_PARAMETERS);
            }
        }
        Ok(request)
    }

    pub fn continuation(&mut self, source: u32) -> Result<Continuation, i64> {
        let cpu = source as usize;
        if cpu >= self.count {
            return Err(INVALID_PARAMETERS);
        }
        if let Some(bootstrap) = self.bootstraps[cpu].take() {
            return Ok(Continuation::Restart(bootstrap));
        }
        if let Transaction::Complete(value) = self.requests[cpu] {
            self.requests[cpu] = Transaction::Empty;
            return Ok(Continuation::Resume(value));
        }
        Ok(Continuation::Wait)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_request_ids_never_wrap_or_reserve_a_target() {
        let Some(mut state) = PowerState::new(2) else {
            panic!("valid topology");
        };
        assert_eq!(state.boot(), Ok(()));
        state.next = u64::MAX;
        assert_eq!(
            state.stage(0, Operation::CpuOn, [1, 0x1000, 0], 0x1000..0x2000),
            Err(INTERNAL_FAILURE)
        );
        assert_eq!(state.affinity(1, 0), 1);
        assert_eq!(state.publish(0), Ok(false));
        assert_eq!(state.pending(), None);
    }
}
