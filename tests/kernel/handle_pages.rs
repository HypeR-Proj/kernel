// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Empty slot and sidecar pages return to the physical allocator before exit.

use crate::kernel::capability::{HandleSidecar, HandleTable, HandleTableStoragePlan};

pub(super) fn run() -> Result<(), &'static str> {
    let mut table = HandleTable::new();
    let mut sidecar = HandleSidecar::<u64>::new();
    let snapshot = table
        .reservation_storage_snapshot_for(64)
        .map_err(|_| "snapshot")?;
    let mut plan = Some(HandleTableStoragePlan::try_new(snapshot).map_err(|_| "slot pages")?);
    let mut index_plan = HandleSidecar::prepare(snapshot).map_err(|_| "index pages")?;
    let reservation = table
        .reserve_with_plan::<64>(&mut plan)
        .map_err(|_| "reserve")?;
    sidecar.install(&mut index_plan);
    if table.take_empty_page().is_some() {
        return Err("reservation did not pin page");
    }
    reservation.abort(&mut table);
    let mut count = 0;
    while let Some(page) = table.take_empty_page() {
        let index = sidecar.detach_empty(page.index());
        let before = crate::kernel::mm::statistics()
            .ok_or("allocator stats")?
            .runtime
            .free_pages;
        drop(page);
        drop(index);
        let after = crate::kernel::mm::statistics()
            .ok_or("allocator stats")?
            .runtime
            .free_pages;
        if after < before + 2 {
            return Err("page pair was not returned to physical allocator");
        }
        count += 1;
    }
    if count == 0 {
        return Err("no empty pages reclaimed");
    }
    let mut cursor = table.begin_teardown().map_err(|_| "teardown")?;
    if table.remove_next(&mut cursor).is_some() {
        return Err("aborted reservation published a handle");
    }
    table.finish_teardown(cursor);
    drop(table.take_retired_storage());
    Ok(())
}
