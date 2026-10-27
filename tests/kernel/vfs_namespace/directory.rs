// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real directory stream identity, replay and independent-progress contracts.

use super::{DirectoryObject, Error, ResourceDomain, ScratchBudget, TestError, check};
use crate::fs::primitives::NodeKind;
use crate::fs::vfs::DirectoryPage;

pub(super) fn run(
    root: &DirectoryObject,
    domain: &ResourceDomain,
    scratch: &ScratchBudget,
) -> Result<(), TestError> {
    root.create("directory-scans", NodeKind::Directory, 0o755, scratch)?;
    let directory = root.open_directory("directory-scans", domain)?;
    for name in ["a", "b", "c", "d", "e", "f", "g", "h", "i"] {
        directory.create(name, NodeKind::File, 0o600, scratch)?;
    }
    let first = directory.begin_read(domain)?;
    let second = directory.begin_read(domain)?;
    let mut page = DirectoryPage::empty();
    first.read_page(0, &mut page)?;
    check(page.len() == 4, "directory stream first page")?;
    let first_cookie = page.next_cookie();
    let mut replay = DirectoryPage::empty();
    first.read_page(0, &mut replay)?;
    check(
        same_page(&page, &replay),
        "copyout replay preserves directory progress",
    )?;
    second.read_page(0, &mut replay)?;
    let second_cookie = replay.next_cookie();
    check(
        first_cookie != 0 && second_cookie != 0 && first_cookie != second_cookie,
        "streams issue distinct continuation authority",
    )?;
    check(
        first.read_page(second_cookie, &mut replay) == Err(Error::InvalidDirectoryCookie),
        "cross-stream cookie rejection",
    )?;
    check(
        first.read_page(u64::MAX, &mut replay) == Err(Error::InvalidDirectoryCookie),
        "forged directory cookie rejection",
    )?;
    first.read_page(first_cookie, &mut page)?;
    let final_cookie = page.next_cookie();
    check(
        page.len() == 4 && final_cookie > first_cookie,
        "second directory page",
    )?;
    check(
        first.read_page(0, &mut replay) == Err(Error::InvalidDirectoryCookie),
        "superseded directory cookie rejection",
    )?;
    second.read_page(second_cookie, &mut replay)?;
    check(
        page.entries()
            .zip(replay.entries())
            .all(|(a, b)| a.name == b.name),
        "independent directory readers see the same second page",
    )?;
    first.read_page(final_cookie, &mut page)?;
    check(
        page.len() == 1 && page.next_cookie() == 0,
        "directory stream EOF",
    )?;
    first.read_page(final_cookie, &mut replay)?;
    check(same_page(&page, &replay), "EOF copyout can be retried")?;
    Ok(())
}

fn same_page(a: &DirectoryPage, b: &DirectoryPage) -> bool {
    a.len() == b.len()
        && a.next_cookie() == b.next_cookie()
        && a.entries()
            .zip(b.entries())
            .all(|(a, b)| a.name == b.name && a.name_length == b.name_length)
}
