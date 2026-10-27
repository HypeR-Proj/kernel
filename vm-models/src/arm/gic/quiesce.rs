// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! VM-wide admission for shared or remote active-register transactions.
//!
//! The owner serializes this state with the interrupt model. Entry reservations
//! precede hardware loading; `bank_saved` follows LR synchronization and hardware
//! detach. Waiting and notification are policy outside this allocation-free
//! runtime state machine. No request owns another caller's execution resources.

use super::mmio::{ModelError, ModelRegister, read_model_register, write_model_register};
use super::{BuildError, RuntimeError, VirtualGic};
use crate::exit::MmioOperation;
use crate::interrupt::VirtualCpuId;
use alloc::vec::Vec;

#[derive(Clone, Copy)]
enum Transaction {
    Register {
        bank: VirtualCpuId,
        register: ModelRegister,
        operation: MmioOperation,
    },
    Deactivate {
        requester: VirtualCpuId,
        interrupt: u32,
        source: Option<u8>,
        split_eoi: bool,
    },
}

#[derive(Clone, Copy)]
struct Request {
    transaction: Transaction,
    result: Option<Result<Option<u64>, ModelError>>,
}

pub struct ActiveAccessQuiesce {
    resident: u64,
    requests: Vec<Option<Request>>,
    prompts: u64,
}

impl ActiveAccessQuiesce {
    pub const fn allocation_requirement(count: u32) -> usize {
        count as usize * core::mem::size_of::<Option<Request>>()
    }

    pub fn try_new(count: u32) -> Result<Self, BuildError> {
        if count == 0 || count > 64 {
            return Err(BuildError::InvalidStoragePlan);
        }
        let mut requests = Vec::new();
        requests
            .try_reserve_exact(count as usize)
            .map_err(|_| BuildError::Allocation)?;
        requests.resize(count as usize, None);
        Ok(Self {
            resident: 0,
            requests,
            prompts: 0,
        })
    }

    pub fn allocation_size(&self) -> usize {
        self.requests.capacity() * core::mem::size_of::<Option<Request>>()
    }

    pub fn gate_closed(&self) -> bool {
        self.requests
            .iter()
            .flatten()
            .any(|request| request.result.is_none())
    }

    /// Atomically reserves a bank before hardware loading. A false return
    /// requires the caller to remain detached and wait for the open predicate.
    pub fn try_enter(&mut self, vcpu: VirtualCpuId) -> bool {
        assert!((vcpu.get() as usize) < self.requests.len());
        if self.gate_closed() {
            return false;
        }
        let bit = 1u64 << vcpu.get();
        assert_eq!(self.resident & bit, 0, "duplicate vGIC bank entry");
        self.resident |= bit;
        true
    }

    /// Registers one trapped access per requester. The requester is still
    /// resident, so this method cannot perform the transaction before detach.
    pub fn begin(
        &mut self,
        requester: VirtualCpuId,
        bank: VirtualCpuId,
        register: ModelRegister,
        operation: MmioOperation,
    ) -> Result<(), RuntimeError> {
        self.begin_transaction(
            requester,
            Transaction::Register {
                bank,
                register,
                operation,
            },
        )
    }

    pub fn begin_deactivate(
        &mut self,
        requester: VirtualCpuId,
        interrupt: u32,
        source: Option<u8>,
        split_eoi: bool,
    ) -> Result<(), RuntimeError> {
        self.begin_transaction(
            requester,
            Transaction::Deactivate {
                requester,
                interrupt,
                source,
                split_eoi,
            },
        )
    }

    fn begin_transaction(
        &mut self,
        requester: VirtualCpuId,
        transaction: Transaction,
    ) -> Result<(), RuntimeError> {
        let Some(slot) = self.requests.get_mut(requester.get() as usize) else {
            return Err(RuntimeError::CorruptState);
        };
        if slot.is_some() || self.resident & (1u64 << requester.get()) == 0 {
            return Err(RuntimeError::CorruptState);
        }
        *slot = Some(Request {
            transaction,
            result: None,
        });
        self.prompts |= self.resident;
        Ok(())
    }

    /// The owner must have synchronized this bank's LRs and detached hardware,
    /// or unwound an entry which never ran a guest instruction. This is the
    /// sole retirement boundary for its entry reservation.
    pub fn bank_saved(&mut self, vcpu: VirtualCpuId, controller: &mut VirtualGic) {
        let bit = 1u64 << vcpu.get();
        assert_ne!(self.resident & bit, 0, "unreserved vGIC bank detach");
        self.resident &= !bit;
        self.finish_quiescent(controller);
    }

    fn all_cpus(&self) -> u64 {
        if self.requests.len() == 64 {
            u64::MAX
        } else {
            (1u64 << self.requests.len()) - 1
        }
    }

    fn finish_quiescent(&mut self, controller: &mut VirtualGic) {
        if self.resident != 0 || !self.gate_closed() {
            return;
        }
        for request in self.requests.iter_mut().flatten() {
            if request.result.is_some() {
                continue;
            }
            request.result = Some(match request.transaction {
                Transaction::Register {
                    bank,
                    register,
                    operation: MmioOperation::Read,
                } => read_model_register(controller, bank, register).map(Some),
                Transaction::Register {
                    bank,
                    register,
                    operation: MmioOperation::Write(value),
                } => write_model_register(controller, bank, register, value).map(|()| None),
                Transaction::Deactivate {
                    requester,
                    interrupt,
                    source,
                    split_eoi,
                } => controller
                    .deactivate(requester, interrupt, source, split_eoi)
                    .map(|()| None)
                    .map_err(ModelError::from),
            });
        }
        // Wake requests and entry waiters. Durable predicates, not these
        // coalesced prompts, decide whether a caller may resume.
        self.prompts |= self.all_cpus();
    }

    pub fn pending(&self, vcpu: VirtualCpuId) -> bool {
        self.requests
            .get(vcpu.get() as usize)
            .is_some_and(Option::is_some)
    }

    pub fn take(&mut self, vcpu: VirtualCpuId) -> Option<Result<Option<u64>, ModelError>> {
        let slot = self.requests.get_mut(vcpu.get() as usize)?;
        let result = slot.as_ref()?.result?;
        *slot = None;
        Some(result)
    }

    pub fn cancel(&mut self, vcpu: VirtualCpuId, controller: &mut VirtualGic) {
        let was_closed = self.gate_closed();
        if let Some(slot) = self.requests.get_mut(vcpu.get() as usize) {
            *slot = None;
        }
        self.finish_quiescent(controller);
        if was_closed && !self.gate_closed() {
            self.prompts |= self.all_cpus();
        }
    }

    pub fn take_prompts(&mut self) -> u64 {
        core::mem::take(&mut self.prompts)
    }
}
