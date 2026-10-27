// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Protection follows canonical content and active process ownership.

use super::{DirectoryObject, Error, FileObject, FileOpenOptions, ScratchBudget, TestError, check};
use crate::authority::Rights;
use crate::fs::primitives::NodeKind;
use crate::fs::vfs::FileProtectionObject;
use {
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind,
    crate::accounting::ResourceLimits,
};
use {crate::object::KernelObject, crate::object::ObjectPublication, crate::object::TransferClass};

pub(super) fn run(
    root: &DirectoryObject,
    domain: &ResourceDomain,
    scratch: &ScratchBudget,
) -> Result<(), TestError> {
    let file = root.create_file("protected", 0o666, domain, Ok::<FileObject, Error>)?;
    file.write(Some(0), b"original")?;
    root.link("protected", root, "protected-alias", scratch)?;
    let writer = root.open_file("protected-alias", domain)?;
    let first = file.protect(domain)?;
    let second = writer.protect(domain)?;
    check(
        first.size() == 8 && first.matches_file(&writer),
        "protection captures size and canonical alias identity",
    )?;
    check(
        writer.write(Some(0), b"changed") == Err(Error::Busy)
            && writer.write(None, b"append") == Err(Error::Busy)
            && writer.resize(0) == Err(Error::Busy),
        "protection excludes existing writable handles",
    )?;
    check(
        matches!(
            root.open_file_with_options(
                "protected-alias",
                &FileOpenOptions::new(Rights::WRITE, 4, 0o666)?,
                domain,
                Ok::<FileObject, Error>,
            ),
            Err(Error::Busy)
        ),
        "protection excludes open truncate",
    )?;
    root.rename("protected", root, "protected-renamed", scratch)?;
    let renamed = root.open_file("protected-renamed", domain)?;
    check(
        first.matches_file(&renamed) && renamed.resize(0) == Err(Error::Busy),
        "rename preserves content protection",
    )?;
    // Namespace removal is independent of content protection on an open node.
    root.remove("protected-alias", NodeKind::File, scratch)?;
    root.remove("protected-renamed", NodeKind::File, scratch)?;
    let mut bytes = [0; 8];
    check(
        first.read_at(0, &mut bytes)? == 8 && &bytes == b"original",
        "protected open content survives unlink",
    )?;
    let replacement = root.create_file("protected", 0o666, domain, Ok::<FileObject, Error>)?;
    replacement.write(Some(0), b"new file")?;
    check(
        !first.matches_file(&replacement),
        "recreated name is not the protected content",
    )?;
    drop(first);
    check(
        writer.resize(0) == Err(Error::Busy),
        "one holder exit preserves another holder protection",
    )?;
    drop(second);
    check(
        writer.write(Some(0), b"released")?.0 == 8,
        "final holder release admits writes",
    )?;
    active_handle_retirement(&file, &writer, domain)?;
    denied_allocation_rolls_back(&file, &writer)?;
    root.remove("protected", NodeKind::File, scratch)?;
    Ok(())
}

fn active_handle_retirement(
    file: &FileObject,
    writer: &FileObject,
    domain: &ResourceDomain,
) -> Result<(), TestError> {
    check(
        FileProtectionObject::TRANSFER_CLASS == TransferClass::Never
            && FileProtectionObject::SUPPORTED_RIGHTS
                .intersection(Rights::DUPLICATE.union(Rights::TRANSFER))
                == Rights::NONE,
        "protection token cannot escape the acquiring process",
    )?;
    let object = FileProtectionObject::try_new(file.protect(domain)?, domain)?;
    let active = ObjectPublication::try_new(object)
        .map_err(Error::Object)?
        .activate()
        .map_err(|_| TestError::Contract("activate protection"))?;
    let pin = active
        .pin::<FileProtectionObject>()
        .ok_or(TestError::Contract("pin protection"))?;
    let mapping = pin.object().protection_for(file)?;
    // Process handle teardown performs this same zero-active transition.
    drop(active);
    check(
        matches!(pin.object().protection_for(file), Err(Error::Busy)),
        "operation pin cannot reacquire a closed protection token",
    )?;
    check(
        writer.resize(0) == Err(Error::Busy),
        "mapping retains content protection after token closes",
    )?;
    drop(mapping);
    writer.resize(0)?;
    check(
        file.len()? == 0,
        "mapping teardown releases protection despite retained object pin",
    )?;
    Ok(())
}

fn denied_allocation_rolls_back(file: &FileObject, writer: &FileObject) -> Result<(), TestError> {
    writer.write(Some(0), b"no snapshot allocation")?;
    let domain = ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED
            .with(ResourceKind::CommittedPages, 0)
            .with(ResourceKind::KernelObjects, 0),
    )
    .map_err(Error::Resource)?;
    // A nonempty file can be protected even with no committed-page allowance:
    // this guard owns identity/metadata, never a full file-data copy.
    let guard = file.protect(&domain)?;
    check(
        matches!(
            FileProtectionObject::try_new(guard, &domain),
            Err(Error::Resource(_))
        ),
        "token quota rejection is propagated",
    )?;
    writer.write(Some(0), b"unprotected")?;
    check(
        domain.usage().total(ResourceKind::KernelMemoryBytes) == 0,
        "failed token construction releases guard and its caller charge",
    )
}
