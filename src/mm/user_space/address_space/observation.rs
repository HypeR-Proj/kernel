// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded resident evidence detached from a stopped process's address space.

use super::super::vmo::CrashPage;
use super::*;

const MAX_MAPPINGS: usize = 1024;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_READ: usize = 64 * 1024;
const MAX_SCANNED_PAGES: usize = 65536;

pub(crate) const RETAINED: u32 = 1;
pub(crate) const PARTIAL: u32 = 2;
pub(crate) const EXCLUDED_SHARED: u32 = 4;
pub(crate) const EXCLUDED_FILE: u32 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CrashMappingKind {
    Anonymous,
    PrivateFile,
    Shared,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CrashMapping {
    pub(crate) base: u64,
    pub(crate) length: u64,
    pub(crate) object_offset: u64,
    pub(crate) permissions: Permissions,
    pub(crate) kind: CrashMappingKind,
    pub(crate) flags: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CrashReadError {
    InvalidRange,
    Unavailable,
    Backing,
}

struct RetainedPage<B: PageBackend, A: MemoryAccount> {
    address: u64,
    page: CrashPage<B, A>,
}

/// Only copied mapping metadata and individually retained physical pages.
///
/// No address space, VMO, file protection, residency grant or hardware lease is
/// retained here. The originating resource domain continues paying for each
/// retained page; dropping this owner releases the selected page references.
pub(crate) struct CrashMemory<B: PageBackend, A: MemoryAccount> {
    mappings: Vec<CrashMapping>,
    pages: Vec<RetainedPage<B, A>>,
    partial: bool,
    mappings_partial: bool,
    _charge: A::Charge,
}

impl<B: PageBackend, A: MemoryAccount> CrashMemory<B, A> {
    pub(crate) fn mapping(&self, index: usize) -> Option<CrashMapping> {
        self.mappings.get(index).copied()
    }

    pub(crate) fn mapping_count(&self) -> usize {
        self.mappings.len()
    }

    /// Physical page payload, conservatively counting aliases separately.
    /// Bounded mapping/page metadata is charged separately to the same domain.
    pub(crate) fn retained_bytes(&self) -> u64 {
        self.pages.len() as u64 * PAGE_SIZE
    }

    pub(crate) fn partial(&self) -> bool {
        self.partial
    }

    pub(crate) fn mappings_partial(&self) -> bool {
        self.mappings_partial
    }

    /// Copies the contiguous retained prefix. A hole before the first byte is
    /// Unavailable, not a fabricated zero page or a request to the filesystem.
    pub(crate) fn read(&self, address: u64, output: &mut [u8]) -> Result<usize, CrashReadError> {
        if output.len() > MAX_READ || address.checked_add(output.len() as u64).is_none() {
            return Err(CrashReadError::InvalidRange);
        }
        let mut copied = 0;
        while copied < output.len() {
            let cursor = address + copied as u64;
            let base = cursor / PAGE_SIZE * PAGE_SIZE;
            let Ok(index) = self.pages.binary_search_by_key(&base, |page| page.address) else {
                return if copied == 0 {
                    Err(CrashReadError::Unavailable)
                } else {
                    Ok(copied)
                };
            };
            let offset = (cursor % PAGE_SIZE) as usize;
            let count = (PAGE_SIZE as usize - offset).min(output.len() - copied);
            self.pages[index]
                .page
                .read(offset, &mut output[copied..copied + count])
                .map_err(|_| CrashReadError::Backing)?;
            copied += count;
        }
        Ok(copied)
    }
}

impl<B: PageBackend, A: MemoryAccount> UserAddressSpace<B, A> {
    /// Closes future mutation and copy admission. Existing write reservations
    /// retain their pins until their syscall transaction has safely completed.
    pub(crate) fn begin_process_stop(&self) {
        self.state.with(|state| state.sealed = true);
    }

    /// The caller must first quiesce every execution context using this space.
    /// The machine-facing wrapper makes that physical precondition explicit.
    pub(crate) fn capture_crash_memory(
        &self,
        max_bytes: u64,
    ) -> SpaceResult<B, A, CrashMemory<B, A>> {
        let snapshot = self.state.with(|state| {
            if !state.sealed || !state.user_writes.is_empty() {
                return Err(AddressSpaceError::Busy);
            }
            Ok(state.mappings.clone())
        })?;
        let mapping_count = snapshot.records.len().min(MAX_MAPPINGS);
        let page_limit = (max_bytes.min(MAX_BYTES) / PAGE_SIZE) as usize;
        let page_capacity =
            snapshot
                .records
                .iter()
                .take(mapping_count)
                .fold(0usize, |count, mapping| {
                    count
                        .saturating_add((mapping.snapshot.range.length() / PAGE_SIZE) as usize)
                        .min(page_limit)
                });
        let bytes = mapping_count
            .checked_mul(size_of::<CrashMapping>())
            .and_then(|bytes| {
                page_capacity
                    .checked_mul(size_of::<RetainedPage<B, A>>())
                    .and_then(|pages| bytes.checked_add(pages))
            })
            .and_then(|bytes| bytes.checked_add(size_of::<CrashMemory<B, A>>()))
            .ok_or(AddressSpaceError::SizeOverflow)?;
        let charge = self
            .account
            .try_charge(MemoryCharge {
                kernel_bytes: bytes as u64,
                ..MemoryCharge::default()
            })
            .map_err(AddressSpaceError::Account)?;
        let mut memory = CrashMemory {
            mappings: Vec::new(),
            pages: Vec::new(),
            partial: snapshot.records.len() > mapping_count,
            mappings_partial: snapshot.records.len() > mapping_count,
            _charge: charge,
        };
        memory
            .mappings
            .try_reserve_exact(mapping_count)
            .map_err(|_| AddressSpaceError::Allocation)?;
        memory
            .pages
            .try_reserve_exact(page_capacity)
            .map_err(|_| AddressSpaceError::Allocation)?;
        let mut scanned = 0;
        for mapping in snapshot.records.iter().take(mapping_count) {
            let writable_private = mapping
                .object
                .crash_writable_is_private(mapping.write_lease.as_ref());
            let kind = match mapping.object.private() {
                Some(view) if view.is_file() => CrashMappingKind::PrivateFile,
                Some(_) => CrashMappingKind::Anonymous,
                None if writable_private => CrashMappingKind::Anonymous,
                None => CrashMappingKind::Shared,
            };
            let before = memory.pages.len();
            let total = mapping.snapshot.range.length() / PAGE_SIZE;
            if kind != CrashMappingKind::Shared {
                for index in 0..total {
                    if memory.pages.len() == page_limit || scanned == MAX_SCANNED_PAGES {
                        break;
                    }
                    scanned += 1;
                    if let Some(page) = mapping.object.retain_crash_page(
                        mapping.snapshot.object_offset + index * PAGE_SIZE,
                        writable_private,
                    ) {
                        memory.pages.push(RetainedPage {
                            address: mapping.snapshot.range.base().get() + index * PAGE_SIZE,
                            page,
                        });
                    }
                }
            }
            let kept = (memory.pages.len() - before) as u64;
            let mut flags = if kept != 0 { RETAINED } else { 0 };
            if kept < total {
                flags |= PARTIAL;
                memory.partial = true;
                flags |= match kind {
                    CrashMappingKind::PrivateFile => EXCLUDED_FILE,
                    CrashMappingKind::Shared => EXCLUDED_SHARED,
                    CrashMappingKind::Anonymous => 0,
                };
            }
            memory.mappings.push(CrashMapping {
                base: mapping.snapshot.range.base().get(),
                length: mapping.snapshot.range.length(),
                object_offset: mapping.snapshot.object_offset,
                permissions: mapping.snapshot.permissions,
                kind,
                flags,
            });
        }
        Ok(memory)
    }
}
