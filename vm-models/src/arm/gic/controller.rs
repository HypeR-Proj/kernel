// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Indexed virtual-GIC state and list-register scheduling.

use alloc::vec::Vec;
use core::num::NonZeroU32;

use {crate::interrupt::VirtualCpuId, crate::interrupt::VirtualInterruptId};

use super::ready::{BoundedVec, EntryIndex, ReadyEntries, ReadyError, ReadyQueue, ReadyRank};

const PRIVATE_INTERRUPT_COUNT: usize = 32;
const MAX_GIC_INTERRUPT_ID: u32 = 1_019;
const SHARED_INTERRUPT_COUNT: usize = MAX_GIC_INTERRUPT_ID as usize + 1 - PRIVATE_INTERRUPT_COUNT;
const MAX_LIST_REGISTERS: usize = 64;
// The durable notification bitmap has one bit per admitted vCPU.
const MAX_VCPUS: u32 = u64::BITS;

/// Guest-visible interrupt identifier in the modeled GIC INTID range.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct GicInterruptId(VirtualInterruptId);

impl GicInterruptId {
    pub const fn constant<const ID: u32>() -> Self {
        const { assert!(ID <= MAX_GIC_INTERRUPT_ID) };
        Self(VirtualInterruptId::constant::<ID>())
    }

    pub const fn new(value: u32) -> Option<Self> {
        if value <= MAX_GIC_INTERRUPT_ID {
            Some(Self(VirtualInterruptId::new(value)))
        } else {
            None
        }
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }

    const fn is_private(self) -> bool {
        self.get() < PRIVATE_INTERRUPT_COUNT as u32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptGroup {
    Group0,
    Group1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptTrigger {
    Level,
    Edge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListState {
    Pending,
    Active,
    PendingActive,
}

impl ListState {
    const fn pending(self) -> bool {
        matches!(self, Self::Pending | Self::PendingActive)
    }

    const fn active(self) -> bool {
        matches!(self, Self::Active | Self::PendingActive)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ListEntry {
    /// `GICv2` SGI originating CPU; ignored by the `GICv3` LR encoding.
    pub source: u8,
    pub interrupt: GicInterruptId,
    pub priority: u8,
    pub group: InterruptGroup,
    pub state: ListState,
    pub request_eoi_maintenance: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterruptSnapshot {
    pub enabled: bool,
    pub routed: bool,
    pub pending: bool,
    pub active: bool,
    pub listed: bool,
    pub priority: u8,
    pub group: InterruptGroup,
    pub trigger: InterruptTrigger,
    pub target: VirtualCpuId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildError {
    Allocation,
    AlreadyConfigured,
    InvalidCpu,
    InvalidListRegisterCount,
    InvalidStoragePlan,
    TooManyInterrupts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    Busy,
    CorruptState,
    InvalidCpu,
    InvalidRoute,
    InvalidSlotCount,
    NotConfigured,
    ResidencyMismatch,
    SnapshotContainsDuplicate,
}

#[derive(Clone, Copy)]
struct DirectorySlot(NonZeroU32);

impl DirectorySlot {
    fn new(index: usize) -> Result<Self, BuildError> {
        let encoded = u32::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .and_then(NonZeroU32::new)
            .ok_or(BuildError::TooManyInterrupts)?;
        Ok(Self(encoded))
    }

    const fn index(self) -> EntryIndex {
        EntryIndex(self.0.get() - 1)
    }
}

struct Interrupt {
    id: GicInterruptId,
    target: VirtualCpuId,
    route_target: VirtualCpuId,
    target_mask: u8,
    route_value: u64,
    sgi_sources: u8,
    sgi_source: u8,
    priority: u8,
    group: InterruptGroup,
    trigger: InterruptTrigger,
    enabled: bool,
    routed: bool,
    pending_command: PendingCommand,
    software_active: bool,
    active_command: Option<bool>,
    list_state: Option<ListState>,
    ready_position: Option<usize>,
    listed_position: Option<usize>,
    maintenance_on_eoi: bool,
    line_asserted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingCommand {
    None,
    Assert,
    Clear,
}

struct EntryStore<'a>(&'a mut [Interrupt]);

impl ReadyEntries for EntryStore<'_> {
    fn rank(&self, index: EntryIndex) -> ReadyRank {
        let entry = &self.0[index.0 as usize];
        ReadyRank {
            active: entry.software_active,
            priority: entry.priority,
            interrupt: entry.id.get(),
        }
    }

    fn position(&self, index: EntryIndex) -> Option<usize> {
        self.0[index.0 as usize].ready_position
    }

    fn set_position(&mut self, index: EntryIndex, position: Option<usize>) {
        self.0[index.0 as usize].ready_position = position;
    }
}

struct VcpuDelivery {
    ready: ReadyQueue,
    listed: BoundedVec<EntryIndex>,
}

struct ValidatedSlots {
    indices: [Option<EntryIndex>; MAX_LIST_REGISTERS],
}

/// Fallible construction phase for a virtual GIC.
///
/// Directory, entry, ready-queue, and LR-residency storage is allocated only
/// here. Configure every interrupt before calling [`Self::finish`]; the
/// returned [`VirtualGic`] has no configuration or capacity-growth API.
pub struct VirtualGicBuilder {
    entries: Vec<Interrupt>,
    entry_limit: Option<usize>,
    private: Vec<Option<DirectorySlot>>,
    shared: Vec<Option<DirectorySlot>>,
    vcpu_count: u32,
}

impl VirtualGicBuilder {
    pub fn new(vcpu_count: u32) -> Result<Self, BuildError> {
        if vcpu_count == 0 || vcpu_count > MAX_VCPUS {
            return Err(BuildError::InvalidCpu);
        }
        let private_count = usize::try_from(vcpu_count)
            .ok()
            .and_then(|count| count.checked_mul(PRIVATE_INTERRUPT_COUNT))
            .ok_or(BuildError::Allocation)?;
        Ok(Self {
            entries: Vec::new(),
            entry_limit: None,
            private: empty_slots(private_count)?,
            shared: empty_slots(SHARED_INTERRUPT_COUNT)?,
            vcpu_count,
        })
    }

    /// Creates a builder whose complete interrupt-entry store is allocated up
    /// front according to an externally admitted storage plan.
    pub fn new_with_entry_capacity(
        vcpu_count: u32,
        entry_capacity: usize,
    ) -> Result<Self, BuildError> {
        let mut builder = Self::new(vcpu_count)?;
        let maximum_entries = builder
            .private
            .len()
            .checked_add(builder.shared.len())
            .ok_or(BuildError::Allocation)?;
        if entry_capacity > maximum_entries {
            return Err(BuildError::InvalidStoragePlan);
        }
        builder
            .entries
            .try_reserve_exact(entry_capacity)
            .map_err(|_| BuildError::Allocation)?;
        builder.entry_limit = Some(entry_capacity);
        Ok(builder)
    }

    pub fn configure(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
        priority: u8,
        group: InterruptGroup,
        trigger: InterruptTrigger,
    ) -> Result<(), BuildError> {
        self.validate_cpu(target)?;
        let directory_offset = if interrupt.is_private() {
            private_offset(target, interrupt).ok_or(BuildError::InvalidCpu)?
        } else {
            shared_offset(interrupt)
        };
        let configured = if interrupt.is_private() {
            self.private.get(directory_offset)
        } else {
            self.shared.get(directory_offset)
        }
        .ok_or(BuildError::InvalidCpu)?
        .is_some();
        if configured {
            return Err(BuildError::AlreadyConfigured);
        }
        if self
            .entry_limit
            .is_some_and(|limit| self.entries.len() == limit)
        {
            return Err(BuildError::InvalidStoragePlan);
        }
        if self.entries.len() == self.entries.capacity() {
            self.entries
                .try_reserve(1)
                .map_err(|_| BuildError::Allocation)?;
        }
        let index = DirectorySlot::new(self.entries.len())?;
        self.entries.push(Interrupt {
            id: interrupt,
            target,
            route_target: target,
            target_mask: 1u8.checked_shl(target.get()).unwrap_or(0),
            route_value: u64::from(target.get()),
            sgi_sources: 0,
            sgi_source: 0,
            priority,
            group,
            trigger,
            enabled: false,
            routed: true,
            pending_command: PendingCommand::None,
            software_active: false,
            active_command: None,
            list_state: None,
            ready_position: None,
            listed_position: None,
            maintenance_on_eoi: false,
            line_asserted: false,
        });
        if interrupt.is_private() {
            self.private[directory_offset] = Some(index);
        } else {
            self.shared[directory_offset] = Some(index);
        }
        Ok(())
    }

    /// Allocates the final fixed-capacity runtime indexes and seals the model.
    pub fn finish(self, list_register_count: usize) -> Result<VirtualGic, BuildError> {
        if list_register_count == 0 || list_register_count > MAX_LIST_REGISTERS {
            return Err(BuildError::InvalidListRegisterCount);
        }
        let cpu_count = usize::try_from(self.vcpu_count).map_err(|_| BuildError::Allocation)?;
        let shared_count = self.shared.iter().flatten().count();
        let mut private_counts = empty_usizes(cpu_count)?;
        for entry in &self.entries {
            if entry.id.is_private() {
                private_counts[entry.target.get() as usize] += 1;
            }
        }
        let mut deliveries = Vec::new();
        deliveries
            .try_reserve_exact(cpu_count)
            .map_err(|_| BuildError::Allocation)?;
        for private_count in private_counts {
            let ready_capacity = private_count
                .checked_add(shared_count)
                .ok_or(BuildError::Allocation)?;
            let ready = ReadyQueue::try_with_capacity(ready_capacity).map_err(map_ready_build)?;
            let listed = BoundedVec::try_new(list_register_count)
                .map_err(|error| map_ready_build(error.into()))?;
            deliveries.push(VcpuDelivery { ready, listed });
        }
        Ok(VirtualGic {
            entries: self.entries,
            private: self.private,
            shared: self.shared,
            deliveries,
            vcpu_count: self.vcpu_count,
            list_register_count,
            distributor_enabled: true,
            reconcile_targets: 0,
        })
    }

    fn validate_cpu(&self, cpu: VirtualCpuId) -> Result<(), BuildError> {
        if cpu.get() < self.vcpu_count {
            Ok(())
        } else {
            Err(BuildError::InvalidCpu)
        }
    }
}

/// Finished virtual-GIC state. Every runtime operation is allocation-free.
pub struct VirtualGic {
    entries: Vec<Interrupt>,
    private: Vec<Option<DirectorySlot>>,
    shared: Vec<Option<DirectorySlot>>,
    deliveries: Vec<VcpuDelivery>,
    vcpu_count: u32,
    list_register_count: usize,
    distributor_enabled: bool,
    reconcile_targets: u64,
}

impl VirtualGic {
    pub const fn vcpu_count(&self) -> u32 {
        self.vcpu_count
    }

    /// Coalesced prompts; actual pending/routing state remains in the model.
    pub fn take_reconcile_targets(&mut self) -> u64 {
        core::mem::take(&mut self.reconcile_targets)
    }

    fn changed(&mut self, index: EntryIndex) {
        let entry = &self.entries[index.0 as usize];
        self.reconcile_targets |= 1u64.checked_shl(entry.target.get()).unwrap_or(0)
            | 1u64.checked_shl(entry.route_target.get()).unwrap_or(0);
    }

    pub fn target(&self, interrupt: GicInterruptId) -> Result<VirtualCpuId, RuntimeError> {
        Ok(self.entries[self.lookup_shared(interrupt)?.0 as usize].target)
    }

    pub fn target_mask(&self, interrupt: GicInterruptId) -> Result<u8, RuntimeError> {
        Ok(self.entries[self.lookup_shared(interrupt)?.0 as usize].target_mask)
    }

    pub fn route_value(&self, interrupt: GicInterruptId) -> Result<u64, RuntimeError> {
        Ok(self.entries[self.lookup_shared(interrupt)?.0 as usize].route_value)
    }

    pub fn set_target_mask(
        &mut self,
        interrupt: GicInterruptId,
        mask: u8,
    ) -> Result<(), RuntimeError> {
        let mask = mask & ((1u16 << self.vcpu_count.min(8)) - 1) as u8;
        let index = self.lookup_shared(interrupt)?;
        self.entries[index.0 as usize].target_mask = mask;
        if mask != 0 {
            self.route(interrupt, VirtualCpuId::new(mask.trailing_zeros()))?;
        }
        let target = self.entries[index.0 as usize].target;
        self.set_routed(interrupt, target, mask != 0)
    }

    pub fn set_route_value(
        &mut self,
        interrupt: GicInterruptId,
        value: u64,
    ) -> Result<(), RuntimeError> {
        let index = self.lookup_shared(interrupt)?;
        // One flat affinity level. IRM selects one eligible CPU, deterministically CPU 0.
        let affinity = value & 0x0000_00ff_00ff_ffff;
        let target = if value & (1 << 31) != 0 {
            Some(0)
        } else if affinity < u64::from(self.vcpu_count) {
            Some(affinity as u32)
        } else {
            None
        };
        self.entries[index.0 as usize].route_value = value & 0x0000_00ff_80ff_ffff;
        if let Some(target) = target {
            self.route(interrupt, VirtualCpuId::new(target))?;
        }
        let current = self.entries[index.0 as usize].target;
        self.set_routed(interrupt, current, target.is_some())
    }

    /// Retains distinct `GICv2` SGI sources until each source has been acknowledged.
    pub fn inject_sgi(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
        source: VirtualCpuId,
    ) -> Result<(), RuntimeError> {
        if interrupt.get() >= 16 || source.get() >= 8 {
            return Err(RuntimeError::InvalidRoute);
        }
        self.validate_cpu(source)?;
        let index = self.lookup(interrupt, target)?;
        self.entries[index.0 as usize].sgi_sources |= 1 << source.get();
        if self.entries[index.0 as usize].list_state.is_none() {
            self.entries[index.0 as usize].pending_command = PendingCommand::Assert;
        }
        self.changed(index);
        self.reconcile_ready(index)
    }

    pub fn sgi_sources(
        &self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
    ) -> Result<u8, RuntimeError> {
        let entry = self.entry(interrupt, target)?;
        Ok(entry.sgi_sources
            | if entry.list_state.is_some_and(ListState::pending) {
                1 << entry.sgi_source
            } else {
                0
            })
    }

    pub fn clear_sgi_sources(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
        sources: u8,
    ) -> Result<(), RuntimeError> {
        let index = self.lookup(interrupt, target)?;
        let entry = &mut self.entries[index.0 as usize];
        entry.sgi_sources &= !sources;
        if entry.list_state.is_some() && sources & (1 << entry.sgi_source) != 0 {
            entry.pending_command = PendingCommand::Clear;
        } else if entry.list_state.is_none() {
            entry.pending_command = if entry.sgi_sources != 0 {
                PendingCommand::Assert
            } else {
                PendingCommand::None
            };
        }
        self.changed(index);
        self.reconcile_ready(index)
    }

    fn settle_unlisted(&mut self, index: EntryIndex) -> Result<(), RuntimeError> {
        let entry = &self.entries[index.0 as usize];
        if entry.list_state.is_some() || entry.software_active || entry.active_command == Some(true)
        {
            return Ok(());
        }
        if entry.target != entry.route_target {
            self.validate_ready_position(index)?;
            let old = entry.target.get() as usize;
            let (entries, deliveries) = (&mut self.entries, &mut self.deliveries);
            deliveries[old]
                .ready
                .remove(index, &mut EntryStore(entries));
            entries[index.0 as usize].target = entries[index.0 as usize].route_target;
            self.changed(index);
        }
        let entry = &mut self.entries[index.0 as usize];
        if entry.sgi_sources != 0
            || (entry.line_asserted && entry.trigger == InterruptTrigger::Level)
        {
            entry.pending_command = PendingCommand::Assert;
        }
        Ok(())
    }

    /// Retires one detached hardware bank before reinitializing its vCPU.
    /// The caller must guarantee the old bank can never execute again.
    pub fn reset_vcpu(
        &mut self,
        vcpu: VirtualCpuId,
        group: InterruptGroup,
        sgi_enabled: bool,
    ) -> Result<(), RuntimeError> {
        self.validate_cpu(vcpu)?;
        let cpu = vcpu.get() as usize;
        self.validate_listed(cpu)?;
        while let Some(index) = self.deliveries[cpu].listed.pop() {
            let entry = &mut self.entries[index.0 as usize];
            if !entry.id.is_private()
                && entry.pending_command == PendingCommand::None
                && entry.list_state.is_some_and(ListState::pending)
            {
                entry.pending_command = PendingCommand::Assert;
            }
            entry.list_state = None;
            entry.listed_position = None;
            entry.software_active = false;
            entry.active_command = None;
            self.settle_unlisted(index)?;
            self.reconcile_ready(index)?;
        }
        // Unlisted active interrupts also belong to the retiring bank.
        for raw in 0..self.entries.len() {
            let entry = &mut self.entries[raw];
            if entry.target != vcpu || entry.id.is_private() || !entry.software_active {
                continue;
            }
            entry.software_active = false;
            entry.active_command = None;
            let index = EntryIndex(raw as u32);
            self.settle_unlisted(index)?;
            self.reconcile_ready(index)?;
        }
        for id in 0..32 {
            let Some(slot) = self.private.get(cpu * 32 + id).and_then(|slot| *slot) else {
                continue;
            };
            let index = slot.index();
            let entry = &mut self.entries[index.0 as usize];
            entry.enabled = sgi_enabled && id < 16;
            entry.routed = true;
            entry.priority = 0x80;
            entry.group = group;
            entry.trigger = if id < 16 {
                InterruptTrigger::Edge
            } else {
                InterruptTrigger::Level
            };
            entry.pending_command = PendingCommand::None;
            entry.software_active = false;
            entry.active_command = None;
            entry.sgi_sources = 0;
            entry.sgi_source = 0;
            entry.line_asserted = false;
            self.reconcile_ready(index)?;
        }
        self.reconcile_targets |= 1u64.checked_shl(vcpu.get()).unwrap_or(0);
        Ok(())
    }

    /// Gates delivery without changing per-interrupt enable or pending state.
    pub fn set_distributor_enabled(&mut self, enabled: bool) {
        self.distributor_enabled = enabled;
        self.reconcile_targets |= u64::MAX
            .checked_shr(64u32.saturating_sub(self.vcpu_count))
            .unwrap_or(0);
    }

    /// Requested heap layout retained by a controller with fixed capacities.
    ///
    /// This is a pre-allocation contract. The caller supplies the exact number
    /// of configured private entries per vCPU and shared entries; construction
    /// must use matching capacities and verify the realized layout before
    /// publication.
    pub fn allocation_requirement(
        vcpu_count: u32,
        private_entries_per_vcpu: usize,
        shared_entries: usize,
        list_register_count: usize,
    ) -> Result<usize, BuildError> {
        if vcpu_count == 0
            || vcpu_count > MAX_VCPUS
            || private_entries_per_vcpu > PRIVATE_INTERRUPT_COUNT
            || shared_entries > SHARED_INTERRUPT_COUNT
            || list_register_count == 0
            || list_register_count > MAX_LIST_REGISTERS
        {
            return Err(BuildError::InvalidStoragePlan);
        }
        let cpu_count = usize::try_from(vcpu_count).map_err(|_| BuildError::Allocation)?;
        let entry_count = private_entries_per_vcpu
            .checked_mul(cpu_count)
            .and_then(|count| count.checked_add(shared_entries))
            .ok_or(BuildError::Allocation)?;
        let private_directory = PRIVATE_INTERRUPT_COUNT
            .checked_mul(cpu_count)
            .and_then(|count| count.checked_mul(core::mem::size_of::<Option<DirectorySlot>>()))
            .ok_or(BuildError::Allocation)?;
        let shared_directory = SHARED_INTERRUPT_COUNT
            .checked_mul(core::mem::size_of::<Option<DirectorySlot>>())
            .ok_or(BuildError::Allocation)?;
        let entries = entry_count
            .checked_mul(core::mem::size_of::<Interrupt>())
            .ok_or(BuildError::Allocation)?;
        let deliveries = cpu_count
            .checked_mul(core::mem::size_of::<VcpuDelivery>())
            .ok_or(BuildError::Allocation)?;
        let ready_entries = private_entries_per_vcpu
            .checked_add(shared_entries)
            .and_then(|count| count.checked_mul(core::mem::size_of::<EntryIndex>()))
            .and_then(|bytes| bytes.checked_mul(cpu_count))
            .ok_or(BuildError::Allocation)?;
        let listed_entries = list_register_count
            .checked_mul(core::mem::size_of::<EntryIndex>())
            .and_then(|bytes| bytes.checked_mul(cpu_count))
            .ok_or(BuildError::Allocation)?;
        entries
            .checked_add(private_directory)
            .and_then(|bytes| bytes.checked_add(shared_directory))
            .and_then(|bytes| bytes.checked_add(deliveries))
            .and_then(|bytes| bytes.checked_add(ready_entries))
            .and_then(|bytes| bytes.checked_add(listed_entries))
            .ok_or(BuildError::Allocation)
    }

    /// Reports heap storage retained by this sealed interrupt model.
    ///
    /// Construction may use temporary scratch vectors, but every capacity
    /// included here remains allocated for the complete controller lifetime.
    pub fn allocation_size(&self) -> Option<usize> {
        let mut bytes = self
            .entries
            .capacity()
            .checked_mul(core::mem::size_of::<Interrupt>())?;
        bytes = bytes.checked_add(
            self.private
                .capacity()
                .checked_mul(core::mem::size_of::<Option<DirectorySlot>>())?,
        )?;
        bytes = bytes.checked_add(
            self.shared
                .capacity()
                .checked_mul(core::mem::size_of::<Option<DirectorySlot>>())?,
        )?;
        bytes = bytes.checked_add(
            self.deliveries
                .capacity()
                .checked_mul(core::mem::size_of::<VcpuDelivery>())?,
        )?;
        for delivery in &self.deliveries {
            bytes = bytes.checked_add(delivery.ready.allocation_size()?)?;
            bytes = bytes.checked_add(delivery.listed.allocation_size()?)?;
        }
        Some(bytes)
    }

    /// Conservatively reports whether saved interrupt state may wake `WFI`.
    ///
    /// CPU-interface priority and group masks are deliberately ignored. That
    /// can cause a harmless resume for a masked interrupt, but never strands a
    /// vCPU because software underestimated architectural deliverability.
    pub fn may_wake_wfi(&self, vcpu: VirtualCpuId) -> Result<bool, RuntimeError> {
        self.validate_cpu(vcpu)?;
        let cpu = vcpu.get() as usize;
        self.validate_listed(cpu)?;
        if !self.distributor_enabled {
            return Ok(false);
        }
        // This is intentionally conservative: priority masks, group enables,
        // VMCR, and APR state are not authoritative in the saved software
        // model. Ignoring them can only cause a harmless early wake.
        if !self.deliveries[cpu].ready.is_empty() {
            return Ok(true);
        }
        Ok(self.deliveries[cpu].listed.iter().copied().any(|index| {
            let entry = &self.entries[index.0 as usize];
            entry.enabled
                && entry.routed
                && match entry.pending_command {
                    PendingCommand::Assert => true,
                    PendingCommand::Clear => false,
                    PendingCommand::None => entry.list_state.is_some_and(ListState::pending),
                }
        }))
    }

    pub fn set_enabled(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
        enabled: bool,
    ) -> Result<(), RuntimeError> {
        let index = self.lookup(interrupt, target)?;
        if enabled && self.entries[index.0 as usize].pending_command == PendingCommand::Assert {
            self.preflight_ready_insert(index)?;
        }
        self.entries[index.0 as usize].enabled = enabled;
        self.changed(index);
        self.reconcile_ready(index)?;
        Ok(())
    }

    /// Selects whether an SPI has any destination without changing ISENABLER.
    pub fn set_routed(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
        routed: bool,
    ) -> Result<(), RuntimeError> {
        let index = self.lookup(interrupt, target)?;
        if routed && self.entries[index.0 as usize].pending_command == PendingCommand::Assert {
            self.preflight_ready_insert(index)?;
        }
        self.entries[index.0 as usize].routed = routed;
        self.changed(index);
        self.reconcile_ready(index)
    }

    pub fn set_priority(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
        priority: u8,
    ) -> Result<(), RuntimeError> {
        let index = self.lookup(interrupt, target)?;
        self.validate_ready_position(index)?;
        self.entries[index.0 as usize].priority = priority;
        self.changed(index);
        let cpu = self.entries[index.0 as usize].target.get() as usize;
        let (entries, deliveries) = (&mut self.entries, &mut self.deliveries);
        deliveries[cpu]
            .ready
            .reprioritize(index, &mut EntryStore(entries));
        Ok(())
    }

    pub fn set_group(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
        group: InterruptGroup,
    ) -> Result<(), RuntimeError> {
        let index = self.lookup(interrupt, target)?;
        self.entries[index.0 as usize].group = group;
        self.changed(index);
        Ok(())
    }

    pub fn set_trigger(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
        trigger: InterruptTrigger,
    ) -> Result<(), RuntimeError> {
        let index = self.lookup(interrupt, target)?;
        let entry = &mut self.entries[index.0 as usize];
        if entry.trigger == trigger {
            return Ok(());
        }
        entry.trigger = trigger;
        if trigger == InterruptTrigger::Level && entry.line_asserted {
            self.inject(interrupt, target)?;
        }
        Ok(())
    }

    pub fn set_maintenance_on_eoi(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
        enabled: bool,
    ) -> Result<(), RuntimeError> {
        self.entry_mut(interrupt, target)?.maintenance_on_eoi = enabled;
        Ok(())
    }

    pub fn route(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
    ) -> Result<(), RuntimeError> {
        if interrupt.is_private() {
            return Err(RuntimeError::InvalidRoute);
        }
        self.validate_cpu(target)?;
        let index = self.lookup_shared(interrupt)?;
        self.entries[index.0 as usize].route_target = target;
        self.changed(index);
        self.settle_unlisted(index)?;
        self.reconcile_ready(index)?;
        Ok(())
    }

    /// Publishes a device level transition; an asserted line is retained across EOI.
    pub fn set_line(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
        asserted: bool,
    ) -> Result<(), RuntimeError> {
        let index = self.lookup(interrupt, target)?;
        if self.entries[index.0 as usize].line_asserted == asserted {
            return Ok(());
        }
        self.entries[index.0 as usize].line_asserted = asserted;
        if asserted {
            self.inject(interrupt, target)
        } else if self.entries[index.0 as usize].trigger == InterruptTrigger::Level {
            self.clear_pending(interrupt, target)
        } else {
            Ok(())
        }
    }

    pub fn inject(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
    ) -> Result<(), RuntimeError> {
        let index = self.lookup(interrupt, target)?;
        if self.entries[index.0 as usize].enabled {
            self.preflight_ready_insert(index)?;
        }
        let entry = &mut self.entries[index.0 as usize];
        entry.pending_command = PendingCommand::Assert;
        self.changed(index);
        self.reconcile_ready(index)?;
        Ok(())
    }

    pub fn clear_pending(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
    ) -> Result<(), RuntimeError> {
        let index = self.lookup(interrupt, target)?;
        let entry = &mut self.entries[index.0 as usize];
        if entry.sgi_sources == 0
            && entry.pending_command != PendingCommand::Assert
            && !entry.list_state.is_some_and(ListState::pending)
        {
            return Ok(());
        }
        entry.sgi_sources = 0;
        entry.pending_command = if entry.list_state.is_some() {
            PendingCommand::Clear
        } else {
            PendingCommand::None
        };
        self.changed(index);
        self.reconcile_ready(index)?;
        Ok(())
    }

    /// Implements a trapped DIR after all potentially owning LR banks were
    /// synchronized. Unlike ICACTIVER, DIR is disabled in combined EOI mode
    /// and the `GICv2` SGI source must identify the active instance.
    pub fn deactivate(
        &mut self,
        requester: VirtualCpuId,
        interrupt: u32,
        source: Option<u8>,
        split_eoi: bool,
    ) -> Result<(), RuntimeError> {
        self.validate_cpu(requester)?;
        if !split_eoi {
            return Ok(());
        }
        let Some(id) = GicInterruptId::new(interrupt) else {
            return Ok(());
        };
        let index = match if id.is_private() {
            self.lookup(id, requester)
        } else {
            self.lookup_shared(id)
        } {
            Ok(index) => index,
            Err(RuntimeError::NotConfigured) => return Ok(()),
            Err(error) => return Err(error),
        };
        let entry = &self.entries[index.0 as usize];
        if interrupt < 16 && source.is_some_and(|source| source != entry.sgi_source) {
            return Ok(());
        }
        self.set_active(id, entry.target, false)
    }

    /// Changes architectural active state without requiring an LR slot.
    /// The caller synchronizes the relevant hardware bank before access; queued
    /// commands also survive a later snapshot from a remotely running owner.
    pub fn set_active(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
        active: bool,
    ) -> Result<(), RuntimeError> {
        let index = self.lookup(interrupt, target)?;
        self.validate_ready_position(index)?;
        if active {
            self.preflight_ready_insert(index)?;
        }
        let entry = &mut self.entries[index.0 as usize];
        if entry.list_state.is_some() {
            entry.active_command = Some(active);
        } else {
            if active && !entry.software_active && interrupt.get() < 16 {
                // ISACTIVER carries no source CPU. Give the manufactured
                // instance the same canonical source in shadow and in an LR.
                entry.sgi_source = 0;
            }
            entry.software_active = active;
            entry.active_command = None;
            if !active {
                self.settle_unlisted(index)?;
            }
        }
        self.changed(index);
        self.reconcile_ready(index)
    }

    pub fn snapshot(
        &self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
    ) -> Result<InterruptSnapshot, RuntimeError> {
        let entry = self.entry(interrupt, target)?;
        let listed_pending = entry.list_state.is_some_and(ListState::pending);
        let pending = match entry.pending_command {
            PendingCommand::Assert => true,
            PendingCommand::Clear => false,
            PendingCommand::None => listed_pending,
        };
        Ok(InterruptSnapshot {
            enabled: entry.enabled,
            routed: entry.routed,
            pending: pending || entry.sgi_sources != 0,
            active: entry.active_command.unwrap_or(
                entry.software_active || entry.list_state.is_some_and(ListState::active),
            ),
            listed: entry.list_state.is_some(),
            priority: entry.priority,
            group: entry.group,
            trigger: entry.trigger,
            target: entry.target,
        })
    }

    /// Replaces the saved hardware LR residency after a complete bank read.
    ///
    /// `slots` must contain exactly the LR count supplied to
    /// [`VirtualGicBuilder::finish`]. The entire snapshot is validated before
    /// state changes, and unpublished Assert/Clear commands are retained for
    /// the following [`Self::refill`] transaction.
    pub fn synchronize(
        &mut self,
        vcpu: VirtualCpuId,
        slots: &[Option<ListEntry>],
    ) -> Result<(), RuntimeError> {
        let validated = self.validate_snapshot_slots(vcpu, slots)?;
        let cpu = vcpu.get() as usize;
        let additions = self.deliveries[cpu]
            .listed
            .iter()
            .filter(|index| {
                let entry = &self.entries[index.0 as usize];
                entry.enabled
                    && entry.pending_command == PendingCommand::Assert
                    && entry.ready_position.is_none()
            })
            .count();
        if additions > self.deliveries[cpu].ready.remaining() {
            return Err(RuntimeError::CorruptState);
        }
        while let Some(index) = self.deliveries[cpu].listed.pop() {
            let entry = &mut self.entries[index.0 as usize];
            entry.list_state = None;
            entry.listed_position = None;
            if !validated.indices.contains(&Some(index)) {
                if let Some(active) = entry.active_command.take() {
                    entry.software_active = active;
                }
                self.settle_unlisted(index)?;
            }
            self.reconcile_ready(index)?;
        }
        for (listed, index) in slots
            .iter()
            .zip(validated.indices)
            .filter_map(|(slot, index)| slot.as_ref().zip(index))
        {
            let entry = &mut self.entries[index.0 as usize];
            entry.list_state = Some(listed.state);
            entry.sgi_source = listed.source;
            if entry.list_state.is_some() {
                entry.listed_position = Some(self.deliveries[cpu].listed.len());
                self.deliveries[cpu]
                    .listed
                    .push(index)
                    .map_err(|error| map_ready_runtime(error.into()))?;
            }
            self.reconcile_ready(index)?;
        }
        Ok(())
    }

    /// Applies pending commands and fills a previously synchronized LR bank.
    ///
    /// The caller must pass the exact identity and `ListState` residency most
    /// recently synchronized with this controller. Metadata fields are
    /// refreshed by this method. A mismatched bank is rejected before either
    /// the caller's slice or controller state changes.
    pub fn refill(
        &mut self,
        vcpu: VirtualCpuId,
        slots: &mut [Option<ListEntry>],
    ) -> Result<usize, RuntimeError> {
        let validated = self.validate_refill_slots(vcpu, slots)?;
        let cpu = vcpu.get() as usize;
        for (slot, index) in slots.iter_mut().zip(validated.indices) {
            let Some(mut listed) = *slot else {
                continue;
            };
            let Some(index) = index else {
                continue;
            };
            let entry = &mut self.entries[index.0 as usize];
            let mut state = Some(listed.state);
            state = apply_pending_command(entry, state);
            state = apply_active_command(entry, state);
            state = apply_disabled_policy(
                entry,
                state,
                self.distributor_enabled && entry.target == entry.route_target,
            );
            let Some(state) = state else {
                entry.list_state = None;
                entry.listed_position = None;
                *slot = None;
                self.settle_unlisted(index)?;
                self.reconcile_ready(index)?;
                continue;
            };
            entry.list_state = Some(state);
            listed.priority = entry.priority;
            listed.group = entry.group;
            listed.state = state;
            listed.request_eoi_maintenance =
                entry.maintenance_on_eoi || !entry.id.is_private() || entry.id.get() < 16;
            *slot = Some(listed);
            self.reconcile_ready(index)?;
        }
        // Prefer active residency over pending-only delivery so normal hardware
        // EOI handling remains available. Trapped DIR also handles overflow
        // instances which cannot occupy an LR.
        if self.deliveries[cpu]
            .ready
            .first()
            .is_some_and(|index| self.entries[index.0 as usize].software_active)
        {
            let active_waiting = self
                .entries
                .iter()
                .filter(|entry| {
                    entry.target == vcpu && entry.software_active && entry.list_state.is_none()
                })
                .count();
            let free = slots.iter().filter(|slot| slot.is_none()).count();
            let mut needed = active_waiting.saturating_sub(free);
            for (slot, index) in slots.iter_mut().zip(validated.indices) {
                if needed == 0 {
                    break;
                }
                if !slot.is_some_and(|listed| listed.state == ListState::Pending) {
                    continue;
                }
                let Some(index) = index else {
                    continue;
                };
                let entry = &mut self.entries[index.0 as usize];
                entry.pending_command = PendingCommand::Assert;
                if entry.id.get() < 16 {
                    entry.sgi_sources |= 1 << entry.sgi_source;
                }
                entry.list_state = None;
                entry.listed_position = None;
                *slot = None;
                self.reconcile_ready(index)?;
                needed -= 1;
            }
        }
        self.rebuild_listed(cpu, &validated)?;
        let mut filled = 0;
        for slot in slots.iter_mut().filter(|slot| slot.is_none()) {
            if !self.distributor_enabled
                && !self.deliveries[cpu]
                    .ready
                    .first()
                    .is_some_and(|index| self.entries[index.0 as usize].software_active)
            {
                break;
            }
            let index = {
                let (entries, deliveries) = (&mut self.entries, &mut self.deliveries);
                deliveries[cpu].ready.pop(&mut EntryStore(entries))
            };
            let Some(index) = index else {
                break;
            };
            let entry = &mut self.entries[index.0 as usize];
            let active = entry.software_active;
            let pending = entry.pending_command == PendingCommand::Assert
                && entry.enabled
                && entry.routed
                && entry.target == entry.route_target
                && self.distributor_enabled
                && (!active
                    || entry.id.get() >= 16
                    || entry.sgi_sources & (1 << entry.sgi_source) != 0);
            if pending {
                entry.pending_command = PendingCommand::None;
            }
            entry.software_active = false;
            if !active {
                entry.sgi_source = if entry.sgi_sources != 0 {
                    entry.sgi_sources.trailing_zeros() as u8
                } else {
                    0
                };
            }
            if pending {
                entry.sgi_sources &= !(1 << entry.sgi_source);
            }
            let state = if active {
                if pending {
                    ListState::PendingActive
                } else {
                    ListState::Active
                }
            } else {
                ListState::Pending
            };
            entry.list_state = Some(state);
            entry.listed_position = Some(self.deliveries[cpu].listed.len());
            self.deliveries[cpu]
                .listed
                .push(index)
                .map_err(|error| map_ready_runtime(error.into()))?;
            *slot = Some(ListEntry {
                source: entry.sgi_source,
                interrupt: entry.id,
                priority: entry.priority,
                group: entry.group,
                state,
                request_eoi_maintenance: entry.maintenance_on_eoi
                    || !entry.id.is_private()
                    || entry.id.get() < 16,
            });
            filled += 1;
        }
        Ok(filled)
    }

    /// Refills the current owner's bank and consumes its already-reflected prompt.
    ///
    /// The caller must own this vCPU and install the returned bank before it
    /// resumes, or stop execution if hardware installation fails. Keep the
    /// controller lock across this operation so later producers publish a new
    /// prompt. Notifications for other vCPUs are retained.
    pub fn refill_current(
        &mut self,
        vcpu: VirtualCpuId,
        slots: &mut [Option<ListEntry>],
    ) -> Result<usize, RuntimeError> {
        let filled = self.refill(vcpu, slots)?;
        self.reconcile_targets &= !(1u64 << vcpu.get());
        Ok(filled)
    }

    fn rebuild_listed(&mut self, cpu: usize, slots: &ValidatedSlots) -> Result<(), RuntimeError> {
        for index in self.deliveries[cpu].listed.iter().copied() {
            self.entries[index.0 as usize].listed_position = None;
        }
        self.deliveries[cpu].listed.clear();
        for index in slots.indices.iter().flatten().copied() {
            if self.entries[index.0 as usize].list_state.is_none() {
                continue;
            }
            self.entries[index.0 as usize].listed_position =
                Some(self.deliveries[cpu].listed.len());
            self.deliveries[cpu]
                .listed
                .push(index)
                .map_err(|error| map_ready_runtime(error.into()))?;
        }
        Ok(())
    }

    fn reconcile_ready(&mut self, index: EntryIndex) -> Result<(), RuntimeError> {
        let entry = &self.entries[index.0 as usize];
        let should = entry.list_state.is_none()
            && (entry.software_active
                || (entry.enabled
                    && entry.routed
                    && entry.pending_command == PendingCommand::Assert));
        let present = entry.ready_position.is_some();
        let cpu = entry.target.get() as usize;
        match (present, should) {
            (false, true) => {
                let (entries, deliveries) = (&mut self.entries, &mut self.deliveries);
                deliveries[cpu]
                    .ready
                    .insert(index, &mut EntryStore(entries))
                    .map_err(map_ready_runtime)?;
            }
            (true, false) => {
                let (entries, deliveries) = (&mut self.entries, &mut self.deliveries);
                deliveries[cpu]
                    .ready
                    .remove(index, &mut EntryStore(entries));
            }
            (true, true) => {
                let (entries, deliveries) = (&mut self.entries, &mut self.deliveries);
                deliveries[cpu]
                    .ready
                    .reprioritize(index, &mut EntryStore(entries));
            }
            _ => {}
        }
        Ok(())
    }

    fn preflight_ready_insert(&self, index: EntryIndex) -> Result<(), RuntimeError> {
        let entry = &self.entries[index.0 as usize];
        if entry.list_state.is_none() && entry.ready_position.is_none() {
            let cpu = entry.target.get() as usize;
            self.deliveries[cpu]
                .ready
                .can_insert()
                .map_err(map_ready_runtime)?;
        }
        Ok(())
    }

    fn validate_ready_position(&self, index: EntryIndex) -> Result<(), RuntimeError> {
        let cpu = self.entries[index.0 as usize].target.get() as usize;
        self.deliveries[cpu]
            .ready
            .contains(index, &EntryView(&self.entries))
            .map(|_| ())
            .map_err(map_ready_runtime)
    }

    fn validate_snapshot_slots(
        &self,
        vcpu: VirtualCpuId,
        slots: &[Option<ListEntry>],
    ) -> Result<ValidatedSlots, RuntimeError> {
        self.validate_cpu(vcpu)?;
        if slots.len() != self.list_register_count {
            return Err(RuntimeError::InvalidSlotCount);
        }
        self.validate_listed(vcpu.get() as usize)?;
        let mut seen = [0u64; 16];
        let mut indices = [None; MAX_LIST_REGISTERS];
        for (position, listed) in slots
            .iter()
            .enumerate()
            .filter_map(|(position, slot)| slot.as_ref().map(|listed| (position, listed)))
        {
            if listed.source >= 8 || (listed.interrupt.get() >= 16 && listed.source != 0) {
                return Err(RuntimeError::ResidencyMismatch);
            }
            let id = listed.interrupt.get() as usize;
            let word = id / 64;
            let bit = 1u64 << (id % 64);
            if seen[word] & bit != 0 {
                return Err(RuntimeError::SnapshotContainsDuplicate);
            }
            seen[word] |= bit;
            indices[position] = Some(self.lookup(listed.interrupt, vcpu)?);
        }
        Ok(ValidatedSlots { indices })
    }

    fn validate_refill_slots(
        &self,
        vcpu: VirtualCpuId,
        slots: &[Option<ListEntry>],
    ) -> Result<ValidatedSlots, RuntimeError> {
        let validated = self.validate_snapshot_slots(vcpu, slots)?;
        let cpu = vcpu.get() as usize;
        let mut resident_count = 0;
        for (slot, index) in slots.iter().zip(validated.indices) {
            let Some(listed) = slot else {
                continue;
            };
            let Some(index) = index else {
                return Err(RuntimeError::ResidencyMismatch);
            };
            let entry = &self.entries[index.0 as usize];
            let Some(position) = entry.listed_position else {
                return Err(RuntimeError::ResidencyMismatch);
            };
            if self.deliveries[cpu].listed.get(position).copied() != Some(index)
                || entry.list_state != Some(listed.state)
            {
                return Err(RuntimeError::ResidencyMismatch);
            }
            resident_count += 1;
        }
        if resident_count != self.deliveries[cpu].listed.len() {
            return Err(RuntimeError::ResidencyMismatch);
        }
        Ok(validated)
    }

    fn validate_listed(&self, cpu: usize) -> Result<(), RuntimeError> {
        for (position, index) in self.deliveries[cpu].listed.iter().copied().enumerate() {
            let entry = self
                .entries
                .get(index.0 as usize)
                .ok_or(RuntimeError::CorruptState)?;
            if entry.target.get() as usize != cpu
                || entry.list_state.is_none()
                || entry.listed_position != Some(position)
                || self.deliveries[cpu]
                    .ready
                    .contains(index, &EntryView(&self.entries))
                    .map_err(map_ready_runtime)?
            {
                return Err(RuntimeError::CorruptState);
            }
        }
        Ok(())
    }

    fn lookup(
        &self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
    ) -> Result<EntryIndex, RuntimeError> {
        self.validate_cpu(target)?;
        let slot = if interrupt.is_private() {
            self.private
                .get(private_offset(target, interrupt).ok_or(RuntimeError::InvalidCpu)?)
        } else {
            self.shared.get(shared_offset(interrupt))
        }
        .and_then(|slot| *slot)
        .ok_or(RuntimeError::NotConfigured)?;
        let index = slot.index();
        let entry = self
            .entries
            .get(index.0 as usize)
            .ok_or(RuntimeError::CorruptState)?;
        if entry.target != target {
            return Err(RuntimeError::NotConfigured);
        }
        Ok(index)
    }

    fn lookup_shared(&self, interrupt: GicInterruptId) -> Result<EntryIndex, RuntimeError> {
        if interrupt.is_private() {
            return Err(RuntimeError::InvalidRoute);
        }
        let slot = self
            .shared
            .get(shared_offset(interrupt))
            .and_then(|slot| *slot)
            .ok_or(RuntimeError::NotConfigured)?;
        let index = slot.index();
        if self.entries.get(index.0 as usize).is_some() {
            Ok(index)
        } else {
            Err(RuntimeError::CorruptState)
        }
    }

    fn entry(
        &self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
    ) -> Result<&Interrupt, RuntimeError> {
        let index = self.lookup(interrupt, target)?;
        self.entries
            .get(index.0 as usize)
            .ok_or(RuntimeError::CorruptState)
    }

    fn entry_mut(
        &mut self,
        interrupt: GicInterruptId,
        target: VirtualCpuId,
    ) -> Result<&mut Interrupt, RuntimeError> {
        let index = self.lookup(interrupt, target)?;
        self.entries
            .get_mut(index.0 as usize)
            .ok_or(RuntimeError::CorruptState)
    }

    fn validate_cpu(&self, cpu: VirtualCpuId) -> Result<(), RuntimeError> {
        if cpu.get() < self.vcpu_count {
            Ok(())
        } else {
            Err(RuntimeError::InvalidCpu)
        }
    }
}

struct EntryView<'a>(&'a [Interrupt]);

impl ReadyEntries for EntryView<'_> {
    fn rank(&self, index: EntryIndex) -> ReadyRank {
        let entry = &self.0[index.0 as usize];
        ReadyRank {
            active: entry.software_active,
            priority: entry.priority,
            interrupt: entry.id.get(),
        }
    }

    fn position(&self, index: EntryIndex) -> Option<usize> {
        self.0[index.0 as usize].ready_position
    }

    fn set_position(&mut self, _index: EntryIndex, _position: Option<usize>) {}
}

fn empty_slots(length: usize) -> Result<Vec<Option<DirectorySlot>>, BuildError> {
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(length)
        .map_err(|_| BuildError::Allocation)?;
    slots.resize(length, None);
    Ok(slots)
}

fn empty_usizes(length: usize) -> Result<Vec<usize>, BuildError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(length)
        .map_err(|_| BuildError::Allocation)?;
    values.resize(length, 0);
    Ok(values)
}

fn private_offset(cpu: VirtualCpuId, interrupt: GicInterruptId) -> Option<usize> {
    usize::try_from(cpu.get())
        .ok()?
        .checked_mul(PRIVATE_INTERRUPT_COUNT)?
        .checked_add(interrupt.get() as usize)
}

const fn shared_offset(interrupt: GicInterruptId) -> usize {
    interrupt.get() as usize - PRIVATE_INTERRUPT_COUNT
}

fn apply_active_command(entry: &mut Interrupt, state: Option<ListState>) -> Option<ListState> {
    let Some(active) = entry.active_command.take() else {
        return state;
    };
    match (active, state.is_some_and(ListState::pending)) {
        (true, true) => Some(ListState::PendingActive),
        (true, false) => Some(ListState::Active),
        (false, true) => Some(ListState::Pending),
        (false, false) => None,
    }
}

fn apply_pending_command(entry: &mut Interrupt, state: Option<ListState>) -> Option<ListState> {
    match entry.pending_command {
        PendingCommand::None => state,
        PendingCommand::Assert if entry.enabled => {
            // One LR can represent only one GICv2 SGI source. Keep other
            // sources queued until this active instance has been deactivated.
            if entry.id.get() < 16
                && state.is_some_and(ListState::active)
                && entry.sgi_sources != 0
                && entry.sgi_sources & (1 << entry.sgi_source) == 0
            {
                return state;
            }
            let state = match state {
                None => None,
                Some(ListState::Active) => Some(ListState::PendingActive),
                pending @ Some(ListState::Pending | ListState::PendingActive) => pending,
            };
            if state.is_some_and(ListState::pending) {
                entry.pending_command = PendingCommand::None;
                if entry.id.get() < 16 {
                    // This pending instance now lives in the LR. Leaving its
                    // source in the software queue would deliver it twice.
                    entry.sgi_sources &= !(1 << entry.sgi_source);
                }
            }
            state
        }
        PendingCommand::Assert => state,
        PendingCommand::Clear => {
            entry.pending_command = PendingCommand::None;
            match state {
                Some(ListState::Pending) => None,
                Some(ListState::PendingActive) => Some(ListState::Active),
                other => other,
            }
        }
    }
}

fn apply_disabled_policy(
    entry: &mut Interrupt,
    state: Option<ListState>,
    distributor_enabled: bool,
) -> Option<ListState> {
    if entry.enabled && entry.routed && distributor_enabled {
        return state;
    }
    match state {
        Some(ListState::Pending) => {
            if entry.id.get() < 16 {
                entry.sgi_sources |= 1 << entry.sgi_source;
            }
            entry.pending_command = PendingCommand::Assert;
            None
        }
        Some(ListState::PendingActive) => {
            if entry.id.get() < 16 {
                entry.sgi_sources |= 1 << entry.sgi_source;
            }
            entry.pending_command = PendingCommand::Assert;
            Some(ListState::Active)
        }
        other => other,
    }
}

const fn map_ready_build(error: ReadyError) -> BuildError {
    match error {
        ReadyError::Allocation => BuildError::Allocation,
        ReadyError::Capacity | ReadyError::CorruptPosition => BuildError::TooManyInterrupts,
    }
}

const fn map_ready_runtime(_error: ReadyError) -> RuntimeError {
    RuntimeError::CorruptState
}
