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
                for address in
                    (overlap.base().get()..overlap.end().get()).step_by(PAGE_SIZE as usize)
                {
                    if let Some(page) = mapping_page(mapping, address)? {
                        visit(MappingUpdate::Map(page));
                    }
                }
            }
        } else {
            // COW/file versions may have several protection fragments. Compare
            // each changed version's leaves, including every alias, while
            // skipping unchanged view owners without scanning their pages.
            for mapping in self.replacement.records.iter() {
                let Some(next) = mapping.object.private() else {
                    continue;
                };
                let old = previous
                    .mappings
                    .records
                    .by_token(mapping.snapshot.token)
                    .ok_or(AddressSpaceError::StaleMapping)?;
                let old_view = old
                    .object
                    .private()
                    .ok_or(AddressSpaceError::StaleMapping)?;
                if core::ptr::eq(&**next, &**old_view) {
                    continue;
                }
                for address in (mapping.snapshot.range.base().get()
                    ..mapping.snapshot.range.end().get())
                    .step_by(PAGE_SIZE as usize)
                {
                    let before = mapping_page(old, address)?;
                    let after = mapping_page(mapping, address)?;
                    if before == after {
                        continue;
                    }
                    visit(match after {
                        Some(page) => MappingUpdate::Map(page),
                        None => MappingUpdate::Unmap {
                            address,
                            length: PAGE_SIZE,
                        },
                    });
                }
            }
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
            if mapping.object.private().is_some_and(|v| v.is_file()) =>
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
