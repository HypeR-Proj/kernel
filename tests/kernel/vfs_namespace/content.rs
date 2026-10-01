// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercise content ownership through independently opened namespace aliases.

use super::{DirectoryObject, Error, FileObject, FileOpenOptions, ScratchBudget, TestError, check};
use crate::kernel::accounting::ResourceDomain;
use crate::kernel::authority::Rights;
use alloc::vec::Vec;
use hyper::fs::NodeKind;

pub(super) fn run(
    root: &DirectoryObject,
    domain: &ResourceDomain,
    scratch: &ScratchBudget,
) -> Result<(), TestError> {
    let file = root.create_file("content-original", 0o666, domain, Ok::<FileObject, Error>)?;
    let mut original = buffer(8192 + 37, 0)?;
    for (offset, byte) in original.iter_mut().enumerate() {
        *byte = (offset % 251) as u8;
    }
    check(
        file.write(Some(0), &original)?.0 == original.len(),
        "content initial write",
    )?;
    root.link("content-original", root, "content-alias", scratch)?;
    let reader = root.open_file("content-original", domain)?;
    let writer = root.open_file("content-alias", domain)?;
    let identity = reader.metadata()?.location.node_id;
    check(
        writer.metadata()?.location.node_id == identity,
        "independent content aliases share node identity",
    )?;
    verify(&reader, &original, "initial contents and length")?;
    verify(&writer, &original, "alias initial contents and length")?;
    let snapshot = reader.readable_snapshot(domain)?;
    let mut expected = buffer(original.len() + 6, 0)?;
    expected.truncate(original.len());
    expected.copy_from_slice(&original);

    let replacement = b"overwrite-across-page";
    check(
        writer.write(Some(4093), replacement)?.0 == replacement.len(),
        "alias overwrite completes",
    )?;
    expected[4093..4093 + replacement.len()].copy_from_slice(replacement);
    verify(&reader, &expected, "reader observes alias overwrite")?;

    let (written, end) = writer.write(None, b"append")?;
    expected.extend_from_slice(b"append");
    check(
        written == 6 && end == expected.len() as u64,
        "append uses current shared length",
    )?;
    verify(&file, &expected, "original handle observes append")?;
    verify(&reader, &expected, "reader length refreshes after append")?;

    writer.resize(4093)?;
    expected.truncate(4093);
    verify(&reader, &expected, "reader observes shortened file")?;
    reader.resize(8192 + 19)?;
    expected.resize(8192 + 19, 0);
    verify(
        &writer,
        &expected,
        "extension zeroes formerly truncated bytes",
    )?;

    let truncate = FileOpenOptions::new(Rights::WRITE, 4, 0o666)?;
    let failed: Result<(), Error> =
        root.open_file_with_options("content-alias", &truncate, domain, |_| {
            Err(Error::InvalidInput)
        });
    check(
        failed == Err(Error::InvalidInput),
        "truncate reports failed handle preparation",
    )?;
    verify(
        &reader,
        &expected,
        "failed truncate preserves all bytes and length",
    )?;
    let truncated =
        root.open_file_with_options("content-alias", &truncate, domain, Ok::<FileObject, Error>)?;
    verify(&reader, &[], "truncate invalidates another handle's length")?;
    verify(&truncated, &[], "new truncate handle sees empty contents")?;
    check(
        snapshot.bytes() == original,
        "snapshot retains its revision across overwrite resize and truncate",
    )?;

    writer.write(None, b"detached")?;
    root.rename("content-original", root, "content-moved", scratch)?;
    let moved = root.open_file("content-moved", domain)?;
    check(
        moved.metadata()?.location.node_id == identity,
        "rename preserves content identity",
    )?;
    verify(
        &moved,
        b"detached",
        "reopen after rename sees current contents",
    )?;
    root.remove("content-alias", NodeKind::File, scratch)?;
    root.remove("content-moved", NodeKind::File, scratch)?;
    verify(
        &reader,
        b"detached",
        "final unlink preserves opened contents",
    )?;

    let recreated = root.create_file("content-original", 0o666, domain, Ok::<FileObject, Error>)?;
    recreated.write(Some(0), b"replacement")?;
    check(
        recreated.metadata()?.location.node_id != identity,
        "recreated pathname has a new content identity",
    )?;
    verify(
        &recreated,
        b"replacement",
        "recreated pathname has fresh contents",
    )?;
    verify(
        &reader,
        b"detached",
        "recreated pathname cannot replace old lease bytes",
    )?;
    root.remove("content-original", NodeKind::File, scratch)?;
    Ok(())
}

fn verify(file: &FileObject, expected: &[u8], label: &'static str) -> Result<(), TestError> {
    // Read through a retained handle after each mutation through another one.
    // Include a sentinel past EOF to detect accidental use of an obsolete size.
    let mut output = buffer(expected.len() + 1, 0xa7)?;
    check(
        file.len()? == expected.len() as u64
            && file.read(0, &mut output)? == expected.len()
            && &output[..expected.len()] == expected
            && output[expected.len()] == 0xa7,
        label,
    )?;
    let mut tail = [0xa7];
    check(
        file.read(expected.len() as u64, &mut tail)? == 0 && tail == [0xa7],
        "content read stops at current EOF",
    )
}

fn buffer(length: usize, value: u8) -> Result<Vec<u8>, TestError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| Error::Allocation)?;
    bytes.resize(length, value);
    Ok(bytes)
}
