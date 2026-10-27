// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Sv39 Native roots and CPU-local translation transitions.
//! Policy retains table storage, logical identifiers, and residency. Kernel
//! upper tables are shared, supervisor-only, and retained for the machine life.

use crate::arch::riscv64::registers;
use core::arch::asm;
use core::marker::PhantomData;
use core::ptr::{read_volatile, write_volatile};
use hyper::mm::PhysicalAddress;
use hyper::sync::atomic::{AtomicU8, AtomicU64, Ordering};

const USER_LIMIT: u64 = 1 << 38;
// Provisional Sv39 layout: reserve the upper user region for system mappings.
// This is independent of ARM64 geometry and can change with the RV profile.
const APPLICATION_LIMIT: u64 = 1 << 37;
const ASID_SHIFT: u64 = 44;
const ASID_MASK: u64 = 0xffff << ASID_SHIFT;
const PPN_MASK: u64 = (1 << 44) - 1;
const PRIVILEGED_ACCESS: u64 = (1 << 18) | (1 << 19);
static ASID_BITS: AtomicU8 = AtomicU8::new(16);
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Edit(hyper::mm::table_edit::Error),
    Allocation,
    InvalidAddress,
    InvalidRange,
    Conflict,
    NotInitialized,
    InvalidLocalState,
}
pub type ContractError = Error;

/// Probes one hart before Native execution is published. SMP admission completes
/// before the first process is built, making the minimum immutable thereafter.
/// Essential-device discovery rejects every enabled CPU without the complete
/// RV64IMAFDC + Zicsr/Zifencei/H/Sstc/Zicbom contract.
pub(crate) fn discover_local() -> bool {
    crate::arch::riscv64::fp::initialize_local();
    let previous: u64;
    let probed: u64;
    // SAFETY: Boot admission keeps interrupts masked and no Native roots exist.
    // Only the ASID changes; the live kernel PPN and Sv39 mode remain intact.
    unsafe {
        asm!(
        "csrr {previous}, satp",
        "or {probe}, {previous}, {mask}",
        "csrw satp, {probe}",
        "csrr {probe}, satp",
        "csrw satp, {previous}",
        "sfence.vma",
        previous = out(reg) previous, probe = out(reg) probed,
        mask = in(reg) ASID_MASK, options(nostack))
    };
    if previous >> 60 != 8 {
        return false;
    }
    let bits = ((probed & ASID_MASK) >> ASID_SHIFT).count_ones() as u8;
    ASID_BITS.fetch_min(bits, Ordering::AcqRel);
    let root = previous & PPN_MASK;
    match KERNEL_ROOT.compare_exchange(0, root, Ordering::Release, Ordering::Acquire) {
        Ok(_) => true,
        Err(existing) => existing == root,
    }
}

pub(crate) fn identifier_bits() -> Result<u8, Error> {
    ensure_initialized()?;
    // Untagged implementations still need software allocation generations.
    let bits = ASID_BITS.load(Ordering::Acquire);
    Ok(if bits == 0 { 8 } else { bits.min(16) })
}
pub(crate) fn user_address_limit() -> Result<u64, Error> {
    ensure_initialized()?;
    Ok(USER_LIMIT)
}
pub(crate) const fn application_address_limit() -> u64 {
    APPLICATION_LIMIT
}
pub(crate) fn assert_kernel_access() -> Result<(), Error> {
    ensure_initialized()?;
    // SAFETY: Clearing SUM/MXR denies supervisor access to U pages and prevents
    // executable-only mappings from becoming readable. Copies use linear aliases.
    unsafe { asm!("csrc sstatus, {bits}", bits = in(reg) PRIVILEGED_ACCESS, options(nostack)) };
    Ok(())
}
fn ensure_initialized() -> Result<(), Error> {
    if KERNEL_ROOT.load(Ordering::Acquire) == 0 {
        Err(Error::NotInitialized)
    } else {
        Ok(())
    }
}
fn hardware_identifier(identifier: u16) -> u64 {
    if ASID_BITS.load(Ordering::Acquire) == 0 {
        0
    } else {
        u64::from(identifier)
    }
}

use hyper::hal::user::{MappingPage, MappingUpdate};
#[derive(Clone, Copy)]
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
    root: PreparedAddressSpace,
    operation: LocalOperation,
}
#[derive(Clone, Copy)]
pub(crate) struct LocalIdentity {
    root: PreparedAddressSpace,
}
pub(crate) struct LocalActivation {
    previous_root: u64,
    _not_send: PhantomData<*mut ()>,
}
struct LocalOwner {
    root: AtomicU64,
    identifier: AtomicU64,
    generation: AtomicU64,
    epoch: AtomicU64,
}
impl LocalOwner {
    const fn new() -> Self {
        Self {
            root: AtomicU64::new(0),
            identifier: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
        }
    }
    fn publish(&self, root: &PreparedAddressSpace, identifier: u16) {
        self.identifier
            .store(u64::from(identifier), Ordering::Relaxed);
        self.generation.store(root.generation, Ordering::Relaxed);
        self.root.store(root.root, Ordering::Release);
    }
}
static LOCAL: [LocalOwner; hyper::config::MAX_CPUS as usize] =
    [const { LocalOwner::new() }; hyper::config::MAX_CPUS as usize];
fn local() -> &'static LocalOwner {
    match LOCAL.get(crate::arch::riscv64::current_cpu_index()) {
        Some(local) => local,
        None => hyper::debug::invariant_failure("riscv64/user_machine::local invariant"),
    }
}
impl PreparedAddressSpace {
    fn root_register(&self, identifier: u16) -> u64 {
        registers::SATP_MODE_SV39 | (hardware_identifier(identifier) << ASID_SHIFT) | self.root
    }
    pub(crate) const fn local_request(&self, operation: LocalOperation) -> LocalRequest {
        LocalRequest {
            root: *self,
            operation,
        }
    }
    pub(crate) const fn local_identity(&self) -> LocalIdentity {
        LocalIdentity { root: *self }
    }
}

/// Builds a private user hierarchy beneath shared, supervisor-only kernel slots.
///
/// # Safety
/// Every allocator result is a new zeroed page, exclusively transferred to this
/// builder and retained through acknowledged retirement. Mapping owners keep
/// each physical user page live. Kernel root slots are frozen after boot.
pub(crate) unsafe fn prepare_host(
    generation: u64,
    mut enumerate: impl FnMut(&mut dyn FnMut(MappingPage)),
    allocator: &mut impl FnMut(usize) -> Option<PhysicalAddress>,
) -> Result<PreparedAddressSpace, Error> {
    ensure_initialized()?;
    if generation == 0 {
        return Err(Error::InvalidRange);
    }
    let root = allocator(0).ok_or(Error::Allocation)?;
    validate_table(root)?;
    let kernel = PhysicalAddress::new(KERNEL_ROOT.load(Ordering::Acquire) << 12);
    for slot in 256..512 {
        let entry = read_entry(kernel, slot)?;
        if entry & registers::PTE_USER != 0 {
            return Err(Error::InvalidAddress);
        }
        write_entry(root, slot, entry)?;
    }
    let mut result = Ok(());
    enumerate(&mut |page| {
        if result.is_ok() {
            result = map_page(root, page, allocator);
        }
    });
    result?;
    // SAFETY: Publish descriptor stores before another hart can acquire a root.
    unsafe { asm!("fence rw, rw", options(nostack)) };
    Ok(PreparedAddressSpace {
        root: root.get() >> 12,
        generation,
    })
}
fn map_page(
    root: PhysicalAddress,
    page: MappingPage,
    allocator: &mut impl FnMut(usize) -> Option<PhysicalAddress>,
) -> Result<(), Error> {
    let descriptor = leaf_descriptor(page)?;
    if descriptor == 0 {
        return Ok(());
    }
    let mut table = root;
    for shift in [30, 21] {
        let slot = ((page.address >> shift) & 511) as usize;
        let entry = read_entry(table, slot)?;
        table = if entry == 0 {
            let child = allocator(0).ok_or(Error::Allocation)?;
            validate_table(child)?;
            write_entry(table, slot, (child.get() >> 2) | registers::PTE_VALID)?;
            child
        } else if entry & 15 == registers::PTE_VALID {
            PhysicalAddress::new((entry >> 10) << 12)
        } else {
            return Err(Error::Conflict);
        };
    }
    let slot = ((page.address >> 12) & 511) as usize;
    if read_entry(table, slot)? != 0 {
        return Err(Error::Conflict);
    }
    write_entry(table, slot, descriptor)
}

fn validate_table(table: PhysicalAddress) -> Result<(), Error> {
    if table.get() == 0 || table.get() & 4095 != 0 || table.get() >= 1 << 36 {
        Err(Error::InvalidAddress)
    } else {
        Ok(())
    }
}
fn table_pointer(table: PhysicalAddress) -> Result<*mut u64, Error> {
    validate_table(table)?;
    Ok(core::ptr::with_exposed_provenance_mut(
        (registers::SV39_LINEAR_BASE + table.get()) as usize,
    ))
}
fn read_entry(table: PhysicalAddress, slot: usize) -> Result<u64, Error> {
    let pointer = table_pointer(table)?;
    // SAFETY: Private callers produce bounded slots and retain the hierarchy.
    Ok(unsafe { read_volatile(pointer.add(slot)) })
}
fn write_entry(table: PhysicalAddress, slot: usize, value: u64) -> Result<(), Error> {
    let pointer = table_pointer(table)?;
    // SAFETY: Only the unpublished builder writes these uniquely owned pages.
    unsafe { write_volatile(pointer.add(slot), value) };
    Ok(())
}
fn read_satp() -> u64 {
    let value: u64;
    // SAFETY: SATP is a readable HS CSR.
    unsafe { asm!("csrr {value}, satp", value = out(reg) value, options(nomem, nostack)) };
    value
}
fn invalidate(identifier: u16) {
    let tag = hardware_identifier(identifier);
    // SAFETY: Caller retains the ID and excludes U execution. rs2 is a register
    // even for tag zero; no user mapping is global. SFENCE also terminates stale
    // speculative page-table walks before the retirement acknowledgement.
    unsafe { asm!("sfence.vma zero, {tag}", tag = in(reg) tag, options(nostack)) };
}
fn invalidate_all_local() {
    // SAFETY: Local lower-EL execution is excluded; a full fence also retires
    // stale walks belonging to leases recycled since this root last ran.
    unsafe { asm!("sfence.vma", options(nostack)) };
}
fn install(root: &PreparedAddressSpace, identifier: u16) {
    // SAFETY: Shared supervisor mappings cover the executing code and stack
    // before and after SATP; the caller retains all hierarchy pages.
    unsafe {
        asm!("csrw satp, {root}", root = in(reg) root.root_register(identifier), options(nostack))
    };
    invalidate(identifier);
    // SAFETY: The caller retains every newly published executable page. Each
    // consuming hart must synchronize its own instruction stream after root
    // replacement as well as initial activation.
    unsafe { asm!("fence.i", options(nostack)) };
    local().publish(root, identifier);
}
/// Installs one retained root on a pinned, interrupt-masked CPU.
///
/// # Safety
/// Caller closes admission against replacement and retains the root and ID for
/// the complete CPU-local token lifetime. Native activations cannot nest.
pub(crate) unsafe fn activate_local(
    root: &PreparedAddressSpace,
    identifier: u16,
    epoch: u64,
) -> LocalActivation {
    if local().root.load(Ordering::Acquire) != 0 {
        hyper::debug::invariant_failure("riscv64/user_machine::activate_local invariant");
    }
    let valid = identifier_bits()
        .is_ok_and(|bits| identifier != 0 && u32::from(identifier) < (1u32 << bits));
    if epoch == 0 || !valid {
        hyper::debug::invariant_failure("invalid Native ASID lease");
    }
    if local().epoch.load(Ordering::Relaxed) != epoch {
        invalidate_all_local();
        local().epoch.store(epoch, Ordering::Relaxed);
    }
    let previous_root = read_satp();
    install(root, identifier);
    LocalActivation {
        previous_root,
        _not_send: PhantomData,
    }
}
/// Restores the predecessor after Native execution and active-root use stop.
///
/// # Safety
/// Token must be consumed on its original pinned, masked CPU under admission.
pub(crate) unsafe fn deactivate_local(activation: LocalActivation) {
    let owner = local();
    if owner.root.load(Ordering::Acquire) == 0 {
        hyper::debug::invariant_failure("riscv64/user_machine::deactivate_local invariant");
    }
    let identifier = owner.identifier.load(Ordering::Relaxed) as u16;
    // Switch first so old user roots cannot start new walks after the fence.
    // SAFETY: The token retains the preceding kernel root and shared mappings.
    unsafe { asm!("csrw satp, {root}", root = in(reg) activation.previous_root, options(nostack)) };
    invalidate(identifier);
    owner.root.store(0, Ordering::Release);
}
pub(crate) fn local_identity_is_active(identity: LocalIdentity) -> bool {
    let owner = local();
    owner.root.load(Ordering::Acquire) == identity.root.root
        && owner.generation.load(Ordering::Relaxed) == identity.root.generation
        && read_satp()
            == identity
                .root
                .root_register(owner.identifier.load(Ordering::Relaxed) as u16)
}
/// Applies a retained root replacement or final invalidation before RPC ack.
///
/// # Safety
/// Caller retains the request root/ID and excludes concurrent U execution;
/// Replace targets an active owner whose old root remains retained until ack.
pub(crate) unsafe fn service_local_request(request: LocalRequest) -> Result<(), Error> {
    match request.operation {
        LocalOperation::Replace => {
            if local().root.load(Ordering::Acquire) == 0 {
                return Err(Error::InvalidLocalState);
            }
            let old = local().identifier.load(Ordering::Relaxed) as u16;
            if local().generation.load(Ordering::Relaxed) != request.root.generation {
                return Err(Error::InvalidLocalState);
            }
            install(&request.root, old);
            invalidate(old);
        }
        LocalOperation::Invalidate => invalidate_all_local(),
    }
    Ok(())
}

/// Copies externally mutable, resident memory without Rust reference aliasing.
///
/// # Safety
/// Ranges must remain resident, valid for length, and nonoverlapping. Kernel
/// aliases, rather than user VAs, must be passed while SUM remains clear.
pub(crate) unsafe fn copy_from_exposed(source: *const u8, destination: *mut u8, length: usize) {
    // SAFETY: Caller retains both ranges. The assembly memory clobber and byte
    // accesses admit concurrent userspace modification without a Rust borrow.
    unsafe {
        asm!(
        "beqz {length}, 3f", "2:", "lbu {byte}, 0({source})", "sb {byte}, 0({destination})",
        "addi {source}, {source}, 1", "addi {destination}, {destination}, 1",
        "addi {length}, {length}, -1", "bnez {length}, 2b", "3:",
        source = inout(reg) source => _, destination = inout(reg) destination => _,
        length = inout(reg) length => _, byte = out(reg) _, options(nostack))
    };
}
/// Copies private bytes to externally mutable memory.
///
/// # Safety
/// The same residency, linear-alias, and nonoverlap contract as the read copy.
pub(crate) unsafe fn copy_to_exposed(source: *const u8, destination: *mut u8, length: usize) {
    // SAFETY: The byte-loop mechanism has the same requirements in either direction.
    unsafe { copy_from_exposed(source, destination, length) };
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
        hyper::mm::table_edit::Editor::new(PhysicalAddress::new(previous.root << 12), &mut storage)
            .map_err(Error::Edit)?;
    let mut result = Ok(());
    enumerate(&mut |update| {
        if result.is_err() {
            return;
        }
        result = (|| match update {
            MappingUpdate::Unmap { address, length } => {
                if address
                    .checked_add(length)
                    .is_none_or(|end| end > USER_LIMIT)
                {
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
    unsafe { asm!("fence rw, rw", options(nostack, preserves_flags)) };
    Ok(PreparedAddressSpace {
        root: root >> 12,
        generation: previous.generation,
    })
}

struct EditStorage<'a, P> {
    pages: &'a mut P,
}
impl<P: hyper::mm::table_edit::Pages> hyper::mm::table_edit::Storage for EditStorage<'_, P> {
    const LEVELS: usize = 3;
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
        if entry & 15 != registers::PTE_VALID {
            return Err(hyper::mm::table_edit::Error::InvalidEntry);
        }
        Ok(Some(PhysicalAddress::new((entry >> 10) << 12)))
    }
    fn link(table: PhysicalAddress) -> u64 {
        (table.get() >> 2) | registers::PTE_VALID
    }
}

fn leaf_descriptor(page: MappingPage) -> Result<u64, Error> {
    if page.address >= USER_LIMIT
        || page.address & 4095 != 0
        || page.physical.get() & 4095 != 0
        || page.physical.get() >= registers::PHYSICAL_ADDRESS_LIMIT
        || (page.writable && !page.readable)
    {
        return Err(Error::InvalidAddress);
    }
    if !page.readable && !page.executable {
        return Ok(0);
    }
    let mut flags =
        registers::PTE_VALID | registers::PTE_USER | registers::PTE_ACCESSED | registers::PTE_DIRTY;
    if page.readable {
        flags |= registers::PTE_READ;
    }
    if page.writable {
        flags |= registers::PTE_WRITE;
    }
    if page.executable {
        flags |= registers::PTE_EXECUTE;
    }
    Ok((page.physical.get() >> 2) | flags)
}
