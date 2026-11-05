// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Large RAMFS files exercise real page allocation and namespace ownership.

use super::{DirectoryObject, Error, FileObject, ScratchBudget, TestError, check};
use crate::accounting::ResourceDomain;
use crate::fs::primitives::NodeKind;
use alloc::vec::Vec;

pub(super) fn run(
    root: &DirectoryObject,
    domain: &ResourceDomain,
    scratch: &ScratchBudget,
) -> Result<(), TestError> {
    const BLOCK: usize = 128 * 1024;
    const LENGTH: usize = 66 * 1024 * 1024 + 13;
    let file = root.create_file("paged-storage", 0o600, domain, Ok::<FileObject, Error>)?;
    let baseline = crate::fs::vfs::ramfs_capacity_bytes();
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(BLOCK)
        .map_err(|_| Error::Allocation)?;
    bytes.resize(BLOCK, 0);
    for offset in (0..LENGTH).step_by(BLOCK) {
        let count = BLOCK.min(LENGTH - offset);
        bytes.fill((offset / BLOCK % 251) as u8);
        check(
            file.write(None, &bytes[..count])? == (count, (offset + count) as u64),
            "large ramfs append crosses contiguous allocation boundaries",
        )?;
    }
    let retained = crate::fs::vfs::ramfs_capacity_bytes() - baseline;
    check(
        file.len()? == LENGTH as u64 && retained == (LENGTH.div_ceil(4096) * 4096) as u64,
        "large ramfs reports page-rounded payload capacity",
    )?;
    let reader = root.open_file("paged-storage", domain)?;
    root.remove("paged-storage", NodeKind::File, scratch)?;
    for offset in (0..LENGTH).step_by(BLOCK) {
        let count = BLOCK.min(LENGTH - offset);
        bytes.fill(0xff);
        check(
            reader.read(offset as u64, &mut bytes)? == count
                && bytes[..count]
                    .iter()
                    .all(|value| *value == (offset / BLOCK % 251) as u8)
                && bytes[count..].iter().all(|value| *value == 0xff),
            "unlinked large ramfs file retains exact bytes and EOF",
        )?;
    }
    file.resize(4093)?;
    file.resize(8197)?;
    check(
        reader.read(4093, &mut bytes[..4104])? == 4104
            && bytes[..4104].iter().all(|value| *value == 0),
        "large ramfs truncate and extension never expose retired page contents",
    )?;
    file.resize(0)?;
    check(
        crate::fs::vfs::ramfs_capacity_bytes() == baseline,
        "ramfs truncate to zero releases payload reservations",
    )?;
    file.write(None, b"retained until final close")?;
    drop(file);
    check(
        crate::fs::vfs::ramfs_capacity_bytes() > baseline,
        "another open handle retains ramfs storage",
    )?;
    drop(reader);
    check(
        crate::fs::vfs::ramfs_capacity_bytes() == baseline,
        "last close of unlinked ramfs file releases storage",
    )
}
