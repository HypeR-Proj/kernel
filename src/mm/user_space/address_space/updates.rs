// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Enumerate translation changes without copying every resident page address.

use super::*;
use hyper::hal::user::{MappingPage, MappingUpdate};

impl<B: PageBackend, A: MemoryAccount> PreparedMappingChange<'_, B, A> {
    /// The retained logical snapshots keep every visited physical page alive.
    /// The eventual commit checks the same epoch again; enumeration never
    /// authorizes publication of a candidate built against a stale base.
    pub(crate) fn visit_updates(
        &self,
        mut visit: impl FnMut(MappingUpdate),
    ) -> SpaceResult<B, A, ()> {
        let previous = self.address_space.snapshot();
        if previous.mapping_epoch != self.base_mapping_epoch {
            return Err(AddressSpaceError::StaleTransaction);
        }
        if let MappingChangeKind::Authority { range } = self.kind {
            visit(MappingUpdate::Unmap {
                address: range.base().get(),
                length: range.length(),
            });
            for mapping in self.replacement.records.overlapping(range) {
                let Some(overlap) = intersection(mapping.snapshot.range, range) else {
                    continue;
                };
                if let Some(view) = mapping.object.shared_file() {
                    for offset in view.resident_offsets() {
                        if let Some(address) = shared_address(mapping, offset).filter(|address| {
                            *address >= overlap.base().get() && *address < overlap.end().get()
                        }) && let Some(page) = mapping_page(mapping, address)?
                        {
                            visit(MappingUpdate::Map(page));
                        }
                    }
                    continue;
                }
                for address in
                    (overlap.base().get()..overlap.end().get()).step_by(PAGE_SIZE as usize)
                {
                    if let Some(page) = mapping_page(mapping, address)? {
                        visit(MappingUpdate::Map(page));
                    }
                }
            }
        } else {
            visit_backing_updates(&previous.mappings, &self.replacement, visit)?;
        }
        Ok(())
    }
}

fn mapping_page<B: PageBackend, A: MemoryAccount>(
    mapping: &Mapping<B, A>,
    address: u64,
) -> SpaceResult<B, A, Option<MappingPage>> {
    let offset = mapping.snapshot.object_offset + address - mapping.snapshot.range.base().get();
    let physical = match mapping.object.physical_page(offset) {
        Ok(page) => page,
        Err(VmoError::BackingNotResident)
            if mapping.object.private().is_some_and(|v| v.is_file())
                || mapping.object.shared_file().is_some() =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    Ok(Some(MappingPage {
        address,
        physical,
        readable: mapping.snapshot.permissions.contains(Access::Read),
        writable: mapping.snapshot.permissions.contains(Access::Write)
            && mapping
                .object
                .writable_backing(offset, PAGE_SIZE as usize)?,
        executable: mapping.snapshot.permissions.contains(Access::Execute),
    }))
}

pub(super) fn visit_backing_updates<B: PageBackend, A: MemoryAccount>(
    previous: &MappingSet<B, A>,
    replacement: &MappingSet<B, A>,
    mut visit: impl FnMut(MappingUpdate),
) -> SpaceResult<B, A, ()> {
    for mapping in replacement.records.iter() {
        if mapping.object.private().is_none() && mapping.object.shared_file().is_none() {
            continue;
        }
        let old = previous
            .records
            .by_token(mapping.snapshot.token)
            .ok_or(AddressSpaceError::StaleMapping)?;
        let unchanged = match (
            mapping.object.private(),
            old.object.private(),
            mapping.object.shared_file(),
            old.object.shared_file(),
        ) {
            (Some(next), Some(old), _, _) => core::ptr::eq(&**next, &**old),
            (_, _, Some(next), Some(old)) => core::ptr::eq(&**next, &**old),
            _ => false,
        };
        if unchanged {
            continue;
        }
        if let (Some(next), Some(before)) = (mapping.object.shared_file(), old.object.shared_file())
        {
            for offset in before.resident_offsets().chain(
                next.resident_offsets()
                    .filter(|offset| !before.has_page((*offset / PAGE_SIZE) as usize)),
            ) {
                if let Some(address) = shared_address(mapping, offset) {
                    visit_leaf_change(old, mapping, address, &mut visit)?;
                }
            }
            continue;
        }
        for address in (mapping.snapshot.range.base().get()..mapping.snapshot.range.end().get())
            .step_by(PAGE_SIZE as usize)
        {
            visit_leaf_change(old, mapping, address, &mut visit)?;
        }
    }
    Ok(())
}

fn shared_address<B: PageBackend, A: MemoryAccount>(
    mapping: &Mapping<B, A>,
    offset: u64,
) -> Option<u64> {
    let relative = offset.checked_sub(mapping.snapshot.object_offset)?;
    (relative < mapping.snapshot.range.length())
        .then(|| mapping.snapshot.range.base().get() + relative)
}
fn visit_leaf_change<B: PageBackend, A: MemoryAccount>(
    old: &Mapping<B, A>,
    mapping: &Mapping<B, A>,
    address: u64,
    visit: &mut impl FnMut(MappingUpdate),
) -> SpaceResult<B, A, ()> {
    let before = mapping_page(old, address)?;
    let after = mapping_page(mapping, address)?;
    if before != after {
        visit(match after {
            Some(page) => MappingUpdate::Map(page),
            None => MappingUpdate::Unmap {
                address,
                length: PAGE_SIZE,
            },
        });
    }
    Ok(())
}
