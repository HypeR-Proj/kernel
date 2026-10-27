// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Per-VM Arm GIC interrupt-controller state.

use hyper::sync::InterruptSpinLock;
use hyper_vm_models::arm::gic::ListEntry;
use hyper_vm_models::exit::MmioOperation;
use hyper_vm_models::interrupt::VirtualCpuId;
use {
    hyper_vm_models::arm::gic::BuildError as VgicBuildError,
    hyper_vm_models::arm::gic::GicInterruptId, hyper_vm_models::arm::gic::InterruptGroup,
    hyper_vm_models::arm::gic::InterruptSnapshot, hyper_vm_models::arm::gic::InterruptTrigger,
    hyper_vm_models::arm::gic::RuntimeError as VgicError, hyper_vm_models::arm::gic::VirtualGic,
    hyper_vm_models::arm::gic::VirtualGicBuilder,
};
use {
    hyper_vm_models::arm::gic::mmio::DecodedRegister,
    hyper_vm_models::arm::gic::mmio::INTERRUPT_COUNT, hyper_vm_models::arm::gic::mmio::ModelError,
    hyper_vm_models::arm::gic::mmio::RegisterState,
    hyper_vm_models::arm::gic::mmio::read_model_register,
    hyper_vm_models::arm::gic::mmio::write_model_register,
};

type ControllerLock = InterruptSpinLock<ControllerState, crate::arch::aarch64::LocalInterruptMask>;

const TIMER_PRIORITY: u8 = 0x80;
const PRIVATE_ENTRIES_PER_VCPU: usize = 32;
const SHARED_ENTRIES: usize = INTERRUPT_COUNT as usize - PRIVATE_ENTRIES_PER_VCPU;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Build(VgicBuildError),
    InvalidInterrupt,
    MissingCapabilities,
    Vgic(VgicError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessError {
    Controller(VgicError),
    Model(ModelError),
}

impl From<VgicBuildError> for Error {
    fn from(error: VgicBuildError) -> Self {
        Self::Build(error)
    }
}

impl From<VgicError> for Error {
    fn from(error: VgicError) -> Self {
        Self::Vgic(error)
    }
}

pub struct VmInterruptController {
    state: ControllerLock,
    timer_interrupt: GicInterruptId,
    vcpu_count: u32,
}

struct ControllerState {
    controller: VirtualGic,
    registers: RegisterState,
    active_access: hyper_vm_models::arm::gic::quiesce::ActiveAccessQuiesce,
}

impl VmInterruptController {
    pub fn allocation_requirement(vcpu_count: u32, list_registers: usize) -> Result<usize, Error> {
        VirtualGic::allocation_requirement(
            vcpu_count,
            PRIVATE_ENTRIES_PER_VCPU,
            SHARED_ENTRIES,
            list_registers,
        )
        .map_err(Error::Build)?
        .checked_add(
            hyper_vm_models::arm::gic::quiesce::ActiveAccessQuiesce::allocation_requirement(
                vcpu_count,
            ),
        )
        .ok_or(Error::Build(VgicBuildError::Allocation))
    }

    pub fn new(
        vcpu_count: u32,
        timer_interrupt: GicInterruptId,
        list_registers: usize,
    ) -> Result<Self, Error> {
        let cpu_count =
            usize::try_from(vcpu_count).map_err(|_| Error::Build(VgicBuildError::Allocation))?;
        let entry_capacity = PRIVATE_ENTRIES_PER_VCPU
            .checked_mul(cpu_count)
            .and_then(|count| count.checked_add(SHARED_ENTRIES))
            .ok_or(Error::Build(VgicBuildError::Allocation))?;
        let expected_allocation = Self::allocation_requirement(vcpu_count, list_registers)?;
        let mut builder = VirtualGicBuilder::new_with_entry_capacity(vcpu_count, entry_capacity)?;
        for index in 0..vcpu_count {
            let vcpu = VirtualCpuId::new(index);
            for id in 0..32 {
                let interrupt = GicInterruptId::new(id).ok_or(Error::InvalidInterrupt)?;
                builder.configure(
                    interrupt,
                    vcpu,
                    TIMER_PRIORITY,
                    if crate::arch::aarch64::vm::vgic::v2::guest_physical().is_some() {
                        InterruptGroup::Group0
                    } else {
                        InterruptGroup::Group1
                    },
                    if id < 16 {
                        InterruptTrigger::Edge
                    } else {
                        InterruptTrigger::Level
                    },
                )?;
            }
        }
        for id in 32..INTERRUPT_COUNT {
            builder.configure(
                GicInterruptId::new(id).ok_or(Error::InvalidInterrupt)?,
                VirtualCpuId::new(0),
                TIMER_PRIORITY,
                if crate::arch::aarch64::vm::vgic::v2::guest_physical().is_some() {
                    InterruptGroup::Group0
                } else {
                    InterruptGroup::Group1
                },
                InterruptTrigger::Level,
            )?;
        }
        let mut controller = builder.finish(list_registers)?;
        if controller.allocation_size().and_then(|size| {
            size.checked_add(
                hyper_vm_models::arm::gic::quiesce::ActiveAccessQuiesce::allocation_requirement(
                    vcpu_count,
                ),
            )
        }) != Some(expected_allocation)
        {
            return Err(Error::Build(VgicBuildError::InvalidStoragePlan));
        }
        for index in 0..vcpu_count {
            let vcpu = VirtualCpuId::new(index);
            controller.set_maintenance_on_eoi(timer_interrupt, vcpu, true)?;
            controller.set_enabled(timer_interrupt, vcpu, true)?;
        }
        if crate::arch::aarch64::vm::vgic::v2::guest_physical().is_some() {
            controller.set_distributor_enabled(false);
            for cpu in 0..vcpu_count {
                for id in 0..16 {
                    controller.set_enabled(
                        GicInterruptId::new(id).ok_or(Error::InvalidInterrupt)?,
                        VirtualCpuId::new(cpu),
                        true,
                    )?;
                }
            }
        }
        let active_access =
            hyper_vm_models::arm::gic::quiesce::ActiveAccessQuiesce::try_new(vcpu_count)
                .map_err(Error::Build)?;
        Ok(Self {
            state: InterruptSpinLock::new(ControllerState {
                controller,
                registers: RegisterState::new(),
                active_access,
            }),
            timer_interrupt,
            vcpu_count,
        })
    }

    /// Requires the caller to have detached and retired the old hardware bank.
    pub fn reset_vcpu(&self, vcpu: VirtualCpuId) -> Result<(), VgicError> {
        self.cancel_active_access(vcpu.get());
        let v2 = crate::arch::aarch64::vm::vgic::v2::guest_physical().is_some();
        self.with(|controller| {
            controller.reset_vcpu(
                vcpu,
                if v2 {
                    InterruptGroup::Group0
                } else {
                    InterruptGroup::Group1
                },
                v2,
            )?;
            controller.set_enabled(self.timer_interrupt, vcpu, true)
        })
    }

    pub fn take_reconcile_targets(&self) -> u64 {
        self.state.with(|state| {
            state.controller.take_reconcile_targets() | state.active_access.take_prompts()
        })
    }

    pub(crate) fn enable_distributor_for_validation(&self) {
        self.state
            .with(|state| state.controller.set_distributor_enabled(true));
    }

    pub const fn timer_interrupt(&self) -> GicInterruptId {
        self.timer_interrupt
    }

    pub const fn vcpu_count(&self) -> u32 {
        self.vcpu_count
    }

    pub fn allocation_size(&self) -> Option<usize> {
        self.state.with(|state| {
            state
                .controller
                .allocation_size()
                .and_then(|size| size.checked_add(state.active_access.allocation_size()))
        })
    }

    pub(crate) fn with<R>(&self, operation: impl FnOnce(&mut VirtualGic) -> R) -> R {
        self.state.with(|state| operation(&mut state.controller))
    }

    pub(crate) fn try_enter(&self, vcpu: VirtualCpuId) -> bool {
        self.state.with(|state| state.active_access.try_enter(vcpu))
    }

    pub(crate) fn abandon_entry(&self, vcpu: VirtualCpuId) {
        self.state
            .with(|state| state.active_access.bank_saved(vcpu, &mut state.controller));
    }

    pub(crate) fn synchronize_detached(
        &self,
        vcpu: VirtualCpuId,
        slots: &[Option<ListEntry>],
    ) -> Result<(), VgicError> {
        self.state.with(|state| {
            state.controller.synchronize(vcpu, slots)?;
            state.active_access.bank_saved(vcpu, &mut state.controller);
            Ok(())
        })
    }

    pub fn entry_gate_closed(&self) -> bool {
        self.state.with(|state| state.active_access.gate_closed())
    }

    pub fn active_access_pending(&self, vcpu: u32) -> bool {
        self.state
            .with(|state| state.active_access.pending(VirtualCpuId::new(vcpu)))
    }

    pub fn take_active_access(&self, vcpu: u32) -> Option<Result<Option<u64>, AccessError>> {
        self.state.with(|state| {
            state
                .active_access
                .take(VirtualCpuId::new(vcpu))
                .map(|result| result.map_err(AccessError::Model))
        })
    }

    pub fn cancel_active_access(&self, vcpu: u32) {
        self.state.with(|state| {
            state
                .active_access
                .cancel(VirtualCpuId::new(vcpu), &mut state.controller)
        });
    }

    pub fn timer_snapshot(&self, vcpu: VirtualCpuId) -> Result<InterruptSnapshot, VgicError> {
        self.state
            .with(|state| state.controller.snapshot(self.timer_interrupt, vcpu))
    }

    pub fn may_wake_wfi(&self, vcpu: VirtualCpuId) -> Result<bool, VgicError> {
        self.state.with(|state| state.controller.may_wake_wfi(vcpu))
    }

    pub(crate) fn deactivate_saved_bank(
        &self,
        vcpu: VirtualCpuId,
        slots: &mut [Option<ListEntry>],
        interrupt: u32,
        source: Option<u8>,
        split_eoi: bool,
    ) -> Result<bool, VgicError> {
        self.state.with(|state| {
            state.controller.synchronize(vcpu, slots)?;
            if split_eoi && (32..1020).contains(&interrupt) {
                state
                    .active_access
                    .begin_deactivate(vcpu, interrupt, source, split_eoi)?;
                return Ok(true);
            }
            state
                .controller
                .deactivate(vcpu, interrupt, source, split_eoi)?;
            state.controller.refill_current(vcpu, slots)?;
            Ok(false)
        })
    }

    pub(crate) fn access_saved_bank(
        &self,
        vcpu: VirtualCpuId,
        slots: &mut [Option<ListEntry>],
        access: hyper_vm_models::arm::gic::mmio::DecodedAccess,
        operation: MmioOperation,
        split_eoi: bool,
    ) -> Result<Option<u64>, AccessError> {
        if let (
            DecodedRegister::Service(
                hyper_vm_models::arm::gic::mmio::ServiceRegister::CpuDeactivateV2,
            ),
            MmioOperation::Write(value),
        ) = (access.register(), operation)
        {
            self.deactivate_saved_bank(
                vcpu,
                slots,
                value as u32 & 0x3ff,
                Some(((value >> 10) & 7) as u8),
                split_eoi,
            )
            .map_err(AccessError::Controller)?;
            return Ok(None);
        }
        self.state.with(|state| {
            state
                .controller
                .synchronize(vcpu, slots)
                .map_err(AccessError::Controller)?;
            let register = access.register();
            let bank = VirtualCpuId::new(access.redistributor().unwrap_or(vcpu.get()));
            // Only local private active registers are authoritative after
            // this bank's synchronization. Shared and remote-private accesses
            // quiesce all banks, because shared IRQ ownership may migrate.
            if let DecodedRegister::Model(model) = register {
                use {
                    hyper_vm_models::arm::gic::mmio::BitmapRegister,
                    hyper_vm_models::arm::gic::mmio::ModelRegisterDescriptor,
                };
                if let ModelRegisterDescriptor::Bitmap {
                    register: BitmapRegister::SetActive | BitmapRegister::ClearActive,
                    first_interrupt,
                } = model.descriptor()
                    && (first_interrupt >= 32 || bank != vcpu)
                {
                    state
                        .active_access
                        .begin(vcpu, bank, model, operation)
                        .map_err(AccessError::Controller)?;
                    // The caller still owns its live bank and must detach
                    // before this request can execute or be completed.
                    return Ok(None);
                }
            }
            let value = match (register, operation) {
                (DecodedRegister::Service(register), MmioOperation::Read) => Some(
                    state
                        .registers
                        .read_for_cpu(register, bank.get(), self.vcpu_count),
                ),
                (DecodedRegister::Service(register), MmioOperation::Write(value)) => {
                    state.registers.write(register, value);
                    if register
                        == hyper_vm_models::arm::gic::mmio::ServiceRegister::DistributorControlV2
                    {
                        state.controller.set_distributor_enabled(value & 1 != 0);
                    }
                    None
                }
                (DecodedRegister::Model(register), MmioOperation::Read) => Some(
                    read_model_register(&state.controller, bank, register)
                        .map_err(AccessError::Model)?,
                ),
                (DecodedRegister::Model(register), MmioOperation::Write(value)) => {
                    write_model_register(&mut state.controller, bank, register, value)
                        .map_err(AccessError::Model)?;
                    None
                }
                (DecodedRegister::Reserved, MmioOperation::Read) => Some(0),
                (DecodedRegister::Reserved, MmioOperation::Write(_)) => None,
            };
            let _ = state
                .controller
                .refill_current(vcpu, slots)
                .map_err(AccessError::Controller)?;
            Ok(value)
        })
    }
}
