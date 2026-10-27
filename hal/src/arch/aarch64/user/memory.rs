// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Owned native-user translation hierarchies and local Arm transitions.
//!
//! Kernel policy supplies and retains every table page. This module only
//! encodes descriptors and performs bounded local register/TLBI sequences.

use core::arch::asm;
use core::marker::PhantomData;
use core::ptr::{read_volatile, write_volatile};

use hyper::mm::{PAGE_SIZE, PhysicalAddress};
use hyper::sync::atomic::{AtomicU64, Ordering};

use {
    crate::arch::aarch64::memory, crate::arch::aarch64::memory::address,
    crate::arch::aarch64::registers,
};
use {
    crate::arch::aarch64::user::contract::UserMachineContractError,
    crate::arch::aarch64::user::contract::UserPagePermissions,
    crate::arch::aarch64::user::contract::UserTranslationRegisters,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Edit(hyper::mm::table_edit::Error),
    Allocation,
    Conflict,
    Contract(UserMachineContractError),
    InvalidAddress,
    InvalidRange,
}

impl From<UserMachineContractError> for Error {
    fn from(error: UserMachineContractError) -> Self {
        Self::Contract(error)
    }
}

use hyper::hal::user::{MappingPage, MappingUpdate};

pub(crate) struct PreparedAddressSpace {
    root: u64,
    generation: u64,
}

#[derive(Clone, Copy)]
pub(crate) enum LocalOperation {
    Replace,
    Invalidate,
}

#[derive(Clone, Copy)]
pub(crate) struct LocalRequest {
    root_register: u64,
    generation: u64,
    operation: LocalOperation,
}

#[derive(Clone, Copy)]
pub(crate) struct LocalIdentity {
    root_register: u64,
    generation: u64,
}

pub(crate) struct LocalActivation {
    installed_identifier: u16,
    previous_root: u64,
    previous_hcr: u64,
    not_send_or_sync: PhantomData<*mut ()>,
}

impl PreparedAddressSpace {
    pub(crate) const fn root_register(&self) -> u64 {
        self.root
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) const fn local_request(&self, operation: LocalOperation) -> LocalRequest {
        LocalRequest {
            root_register: self.root_register(),
            generation: self.generation(),
            operation,
        }
    }

    pub(crate) const fn local_identity(&self) -> LocalIdentity {
        LocalIdentity {
            root_register: self.root_register(),
            generation: self.generation(),
        }
    }
}

pub(crate) fn local_identity_is_active(identity: LocalIdentity) -> bool {
    let current: u64;
    // SAFETY: TTBR0_EL2 is readable at EL2 without side effects.
    unsafe {
        asm!("mrs {current}, TTBR0_EL2", current=out(reg) current, options(nomem,nostack,preserves_flags));
    }
    current & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT == identity.root_register
        && local_generation().load(Ordering::Acquire) == identity.generation
}

/// Builds an immutable VHE EL2&0 stage-1 root.
///
/// # Safety
///
/// Each allocator result must be a uniquely owned, zeroed, linearly mapped
/// table block with the requested order and natural alignment. The caller
/// retains every block until acknowledged retirement of the returned root.
pub(crate) unsafe fn prepare_address_space(
    generation: u64,
    mut enumerate: impl FnMut(&mut dyn FnMut(MappingPage)),
    allocator: &mut impl FnMut(usize) -> Option<PhysicalAddress>,
) -> Result<PreparedAddressSpace, Error> {
    if generation == 0 {
        return Err(Error::Contract(UserMachineContractError::InvalidGeneration));
    }
    let root = allocator(0).ok_or(Error::Allocation)?;
    validate_table(root)?;
    let mut builder = Builder { root, allocator };
    let mut result = Ok(());
    enumerate(&mut |page| {
        if result.is_ok() {
            result = builder.map(page);
        }
    });
    result?;
    // Complete descriptor publication before a Release-published root token.
    // SAFETY: DSB has no pointer operand and orders builder-owned table stores.
    unsafe { asm!("dsb ishst", options(nostack, preserves_flags)) };
    Ok(PreparedAddressSpace {
        root: root.get(),
        generation,
    })
}

struct Builder<'allocator, Allocator> {
    root: PhysicalAddress,
    allocator: &'allocator mut Allocator,
}

impl<Allocator: FnMut(usize) -> Option<PhysicalAddress>> Builder<'_, Allocator> {
    fn map(&mut self, page: MappingPage) -> Result<(), Error> {
        let descriptor = leaf_descriptor(page)?;
        if descriptor == 0 {
            return Ok(());
        }
        self.map_stage1(page.address, descriptor)
    }

    fn map_stage1(&mut self, address: u64, descriptor: u64) -> Result<(), Error> {
        let mut table = self.root;
        for level in 0..3 {
            let index = stage1_index(address, level);
            table = self.descend(table, index, registers::STAGE1_DESC_TABLE_OR_PAGE)?;
        }
        write_leaf(table, stage1_index(address, 3), descriptor)
    }

    fn descend(
        &mut self,
        table: PhysicalAddress,
        index: usize,
        table_kind: u64,
    ) -> Result<PhysicalAddress, Error> {
        let entry = read_entry(table, index)?;
        if entry & registers::TRANSLATION_DESC_TYPE_MASK == table_kind {
            return Ok(PhysicalAddress::new(
                entry & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT,
            ));
        }
        if entry != 0 {
            return Err(Error::Conflict);
        }
        let child = (self.allocator)(0).ok_or(Error::Allocation)?;
        validate_table(child)?;
        write_entry(table, index, child.get() | table_kind)?;
        Ok(child)
    }
}

fn write_leaf(table: PhysicalAddress, index: usize, descriptor: u64) -> Result<(), Error> {
    let current = read_entry(table, index)?;
    if current != 0 && current != descriptor {
        return Err(Error::Conflict);
    }
    write_entry(table, index, descriptor)
}

/// Installs a root locally after kernel admission has excluded an update cut.
///
/// # Safety
///
/// The complete hierarchy and identifier must remain retained, execution must
/// be pinned to this CPU, and kernel policy must serialize this transition
/// against address-space update admission on the same CPU.
pub(crate) unsafe fn activate_local(
    root: &PreparedAddressSpace,
    identifier: u16,
    epoch: u64,
) -> LocalActivation {
    let index = crate::arch::aarch64::current_cpu_index();
    let seen = &LOCAL_EPOCH[index];
    if epoch == 0 || local_generation().load(Ordering::Acquire) != 0 {
        hyper::debug::invariant_failure("aarch64/user_machine::activate_local invariant");
    }
    let previous_root: u64;
    let previous_hcr: u64;
    // SAFETY: Admission excludes another local Native activation.
    unsafe {
        asm!(
            "mrs {root}, TTBR0_EL2",
            "mrs {hcr}, HCR_EL2",
            root=out(reg) previous_root,
            hcr=out(reg) previous_hcr,
            options(nomem,nostack,preserves_flags)
        );
    }
    let host_mode = registers::HCR_EL2_E2H | registers::HCR_EL2_TGE;
    if previous_root != memory::idle_lower_root() || previous_hcr & host_mode != host_mode {
        hyper::debug::invariant_failure("native activation requires the idle host root");
    }
    if seen.load(Ordering::Relaxed) != epoch {
        invalidate_all_local();
        seen.store(epoch, Ordering::Relaxed);
    }
    let operand = u64::from(identifier) << registers::TTBR_ASID_SHIFT;
    let encoded = encode_root(root, identifier);
    // SAFETY: The installed empty root uses ASID zero, so no walk can refill
    // the nonzero target tag after invalidation. Admission retains its lease.
    unsafe {
        asm!(
            "tlbi aside1, {operand}",
            "dsb ish",
            "msr TTBR0_EL2, {root}",
            "isb",
            operand=in(reg) operand,
            root=in(reg) encoded,
            options(nostack,preserves_flags)
        );
    }
    local_generation().store(root.generation, Ordering::Release);
    LocalActivation {
        installed_identifier: identifier,
        previous_root,
        previous_hcr,
        not_send_or_sync: PhantomData,
    }
}

static LOCAL_EPOCH: [AtomicU64; hyper::config::MAX_CPUS as usize] =
    [const { AtomicU64::new(0) }; hyper::config::MAX_CPUS as usize];
static LOCAL_GENERATION: [AtomicU64; hyper::config::MAX_CPUS as usize] =
    [const { AtomicU64::new(0) }; hyper::config::MAX_CPUS as usize];

fn local_generation() -> &'static AtomicU64 {
    &LOCAL_GENERATION[crate::arch::aarch64::current_cpu_index()]
}

fn encode_root(root: &PreparedAddressSpace, identifier: u16) -> u64 {
    let capabilities = match crate::arch::aarch64::user::execution_capabilities() {
        Ok(capabilities) => capabilities,
        Err(_) => hyper::debug::invariant_failure("native user capabilities disappeared"),
    };
    match UserTranslationRegisters::new(capabilities, root.root, identifier, root.generation) {
        Ok(registers) => registers.root_register(),
        Err(_) => hyper::debug::invariant_failure("invalid native translation lease"),
    }
}

fn invalidate_all_local() {
    // SAFETY: vectors.S restores HCR.E2H=TGE=1 before every host handler,
    // including RPCs interrupting a guest. VMALLE1 therefore targets the host
    // EL2&0 regime, not the guest EL1&0 regime. Lower-EL execution is excluded;
    // this conservative flush ends old walks before retirement acknowledgement.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags)
        )
    };
}

/// Leaves one local native translation interval and restores its predecessor.
///
/// # Safety
///
/// This must run on the same pinned CPU which created `activation`, after
/// lower-EL execution has stopped and while update admission remains excluded.
pub(crate) unsafe fn deactivate_local(activation: LocalActivation) {
    let operand = u64::from(activation.installed_identifier) << registers::TTBR_ASID_SHIFT;
    // SAFETY: Activation retained the permanently empty predecessor root and
    // verified its host HCR. Disconnect the departing hierarchy before TLBI:
    // leaving it installed would allow speculative refills after invalidation.
    // Keep the host regime until its ASID invalidation has completed.
    unsafe {
        asm!(
            "msr TTBR0_EL2, {root}",
            "isb",
            "tlbi aside1, {operand}",
            "dsb ish",
            "msr HCR_EL2, {hcr}",
            "isb",
            operand=in(reg) operand,
            root=in(reg) activation.previous_root,
            hcr=in(reg) activation.previous_hcr,
            options(nostack,preserves_flags)
        );
    }
    local_generation().store(0, Ordering::Release);
}

/// Replaces the currently admitted native root while preserving its original
/// predecessor for the higher-level activation token.
///
/// # Safety
///
/// Kernel policy must prove the current CPU is an active target, keep both
/// roots retained, and hold the address-space admission gate closed.
pub(crate) unsafe fn replace_local(root: &PreparedAddressSpace) {
    if local_generation().load(Ordering::Acquire) != root.generation {
        hyper::debug::invariant_failure("native replacement changed translation owner");
    }
    let current: u64;
    // SAFETY: The admitted local run retains its ASID lease through replacement.
    unsafe {
        asm!("mrs {current}, TTBR0_EL2", current=out(reg) current, options(nostack, preserves_flags))
    };
    let identifier = (current >> registers::TTBR_ASID_SHIFT) as u16;
    let encoded = encode_root(root, identifier);
    let operand = u64::from(identifier) << registers::TTBR_ASID_SHIFT;
    let disconnected = memory::idle_lower_root() | operand;
    // SAFETY: The update cut retains both hierarchies and excludes EL0. Break
    // the old root's reachability before invalidating its ASID; otherwise old
    // speculative walks can refill the TLB between TLBI and the TTBR write.
    // The permanent empty root cannot populate lower translations. ISB makes
    // that break effective, and DSB completes retirement before the new root
    // becomes reachable. Kernel execution uses the unchanged upper hierarchy.
    unsafe {
        asm!(
            "msr TTBR0_EL2, {disconnected}",
            "isb",
            "tlbi aside1, {operand}",
            "dsb ish",
            "msr TTBR0_EL2, {root}",
            "isb",
            disconnected=in(reg) disconnected,
            operand=in(reg) operand,
            root=in(reg) encoded,
            options(nostack, preserves_flags)
        );
    }
}

/// Applies a retained root replacement or invalidation before RPC acknowledgement.
///
/// # Safety
/// The publisher retains old and new hierarchies and excludes lower-EL use.
pub(crate) unsafe fn service_local_request(request: LocalRequest) -> Result<(), Error> {
    let root = PreparedAddressSpace {
        root: request.root_register,
        generation: request.generation,
    };
    match request.operation {
        LocalOperation::Replace => {
            // SAFETY: The kernel validated this active owner and mapping epoch.
            unsafe { replace_local(&root) };
        }
        // Invalidation targets only an inactive owner. Its hierarchy is no
        // longer reachable from TTBR0; refills belonging to another retained
        // active owner do not compromise this request's retirement.
        LocalOperation::Invalidate => invalidate_all_local(),
    }
    Ok(())
}

fn stage1_index(virtual_address: u64, level: usize) -> usize {
    registers::stage1_table_index(virtual_address, level, address::STAGE1_VA_BITS)
}

fn validate_table(table: PhysicalAddress) -> Result<(), Error> {
    memory::linear_page_address(table)
        .map(|_| ())
        .ok_or(Error::InvalidAddress)
}

fn read_entry(table: PhysicalAddress, index: usize) -> Result<u64, Error> {
    let pointer = table_pointer(table)?;
    // SAFETY: The table hierarchy is retained and immutable except for the
    // current exclusive builder before publication.
    Ok(unsafe { read_volatile(pointer.add(index)) })
}

fn write_entry(table: PhysicalAddress, index: usize, value: u64) -> Result<(), Error> {
    let pointer = table_pointer(table)?;
    // SAFETY: Only the unpublished builder writes this retained table page.
    unsafe { write_volatile(pointer.add(index), value) };
    Ok(())
}

fn table_pointer(table: PhysicalAddress) -> Result<*mut u64, Error> {
    memory::linear_page_address(table)
        .map(core::ptr::with_exposed_provenance_mut::<u64>)
        .ok_or(Error::InvalidAddress)
}

/// Builds a candidate hierarchy while leaving the retained old root immutable.
///
/// # Safety
/// The caller retains the old hierarchy and supplies exclusive copied/new
/// pages. Both table and data owners survive acknowledged root retirement.
pub(crate) unsafe fn edit_address_space(
    previous: &PreparedAddressSpace,
    mut enumerate: impl FnMut(&mut dyn FnMut(MappingUpdate)),
    pages: &mut impl hyper::mm::table_edit::Pages,
) -> Result<PreparedAddressSpace, Error> {
    let mut storage = EditStorage { pages };
    let mut editor =
        hyper::mm::table_edit::Editor::new(PhysicalAddress::new(previous.root), &mut storage)
            .map_err(Error::Edit)?;
    let mut result = Ok(());
    enumerate(&mut |update| {
        if result.is_err() {
            return;
        }
        result = (|| match update {
            MappingUpdate::Unmap { address, length } => {
                let limit =
                    crate::arch::aarch64::user::execution_capabilities()?.user_address_limit();
                if address.checked_add(length).is_none_or(|end| end > limit) {
                    return Err(Error::InvalidAddress);
                }
                editor.clear(address, length).map_err(Error::Edit)
            }
            MappingUpdate::Map(page) => {
                let descriptor = leaf_descriptor(page)?;
                editor.set(page.address, descriptor).map_err(Error::Edit)
            }
        })();
    });
    result?;
    let root = editor.root().get();
    // SAFETY: Complete candidate descriptor stores before root publication.
    unsafe { asm!("dsb ishst", options(nostack, preserves_flags)) };
    Ok(PreparedAddressSpace {
        root,
        generation: previous.generation,
    })
}

struct EditStorage<'a, P> {
    pages: &'a mut P,
}
impl<P: hyper::mm::table_edit::Pages> hyper::mm::table_edit::Storage for EditStorage<'_, P> {
    const LEVELS: usize = 4;
    fn read(
        &self,
        table: PhysicalAddress,
        slot: usize,
    ) -> Result<u64, hyper::mm::table_edit::Error> {
        if slot >= 512 {
            return Err(hyper::mm::table_edit::Error::InvalidRange);
        }
        read_entry(table, slot).map_err(|_| hyper::mm::table_edit::Error::Storage)
    }
    fn write(
        &mut self,
        table: PhysicalAddress,
        slot: usize,
        value: u64,
    ) -> Result<(), hyper::mm::table_edit::Error> {
        if slot >= 512 {
            return Err(hyper::mm::table_edit::Error::InvalidRange);
        }
        write_entry(table, slot, value).map_err(|_| hyper::mm::table_edit::Error::Storage)
    }
    fn writable(
        &mut self,
        old: Option<PhysicalAddress>,
    ) -> Result<PhysicalAddress, hyper::mm::table_edit::Error> {
        let table = self
            .pages
            .writable(old)
            .ok_or(hyper::mm::table_edit::Error::Storage)?;
        validate_table(table).map_err(|_| hyper::mm::table_edit::Error::Storage)?;
        Ok(table)
    }
    fn discard(&mut self, table: PhysicalAddress) -> Result<(), hyper::mm::table_edit::Error> {
        if self.pages.discard(table) {
            Ok(())
        } else {
            Err(hyper::mm::table_edit::Error::Storage)
        }
    }
    fn child(entry: u64) -> Result<Option<PhysicalAddress>, hyper::mm::table_edit::Error> {
        if entry == 0 {
            return Ok(None);
        }
        if entry & registers::TRANSLATION_DESC_TYPE_MASK != registers::STAGE1_DESC_TABLE_OR_PAGE {
            return Err(hyper::mm::table_edit::Error::InvalidEntry);
        }
        Ok(Some(PhysicalAddress::new(
            entry & registers::TRANSLATION_DESC_ADDRESS_MASK_48BIT,
        )))
    }
    fn link(table: PhysicalAddress) -> u64 {
        table.get() | registers::STAGE1_DESC_TABLE_OR_PAGE
    }
}

fn leaf_descriptor(page: MappingPage) -> Result<u64, Error> {
    if !page.address.is_multiple_of(PAGE_SIZE)
        || !page.physical.get().is_multiple_of(PAGE_SIZE)
        || page.address
            >= crate::arch::aarch64::user::execution_capabilities()?.user_address_limit()
        || page.physical.get() >= address::physical_address_limit()
    {
        return Err(Error::InvalidAddress);
    }
    let permissions = UserPagePermissions::new(page.readable, page.writable, page.executable)?;
    // PROT_NONE retains logical backing but must not install an accessible leaf.
    Ok(if page.readable {
        permissions.stage1_descriptor(page.physical.get())
    } else {
        0
    })
}
