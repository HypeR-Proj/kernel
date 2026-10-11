// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Builders retain their selected loader bytes independently of open files.

use alloc::vec::Vec;

use super::{DirectoryObject, Error, FileObject, ScratchBudget, TestError, check};
use crate::kernel::accounting::{ResourceDomain, ResourceDomainObject};
use crate::kernel::process::{
    LoaderError, ProcessBuilder, ProcessBuilderError, TaskFactory, TaskGroup, TaskGroupObject,
};
use hyper::fs::NodeKind;

pub(super) fn run(
    root: &DirectoryObject,
    domain: &ResourceDomain,
    scratch: &ScratchBudget,
) -> Result<(), TestError> {
    let factory = TaskFactory::try_new(domain)
        .map_err(|_| TestError::Contract("loader test task factory"))?;
    let group =
        TaskGroup::try_new(domain).map_err(|_| TestError::Contract("loader test task group"))?;
    let group = TaskGroupObject::try_publication(group)
        .map_err(|_| TestError::Contract("loader test group publication"))?;
    let sponsor = ResourceDomainObject::try_publication(domain.clone())
        .map_err(|_| TestError::Contract("loader test domain publication"))?;
    let main = root.create_file("loader-test-main", 0o755, domain, Ok::<FileObject, Error>)?;
    main.write(
        Some(0),
        b"Application format belongs to the selected loader",
    )?;
    let loader = root.create_file("loader-test-entry", 0o755, domain, Ok::<FileObject, Error>)?;
    let bytes = loader_image()?;
    loader.write(Some(0), &bytes)?;

    let builder = ProcessBuilder::try_publication(
        &factory,
        group.object(),
        sponsor.object(),
        prepare_file(root, domain)?,
        &loader,
    )
    .map_err(|_| TestError::Contract("loader test builder creation"))?;
    builder
        .object()
        .set_name("selected-loader")
        .map_err(|_| TestError::Contract("loader test builder name"))?;
    // A pathname replacement and final File drop cannot replace the captured
    // executable bytes. The deliberately non-ELF main must not be parsed here.
    loader.write(Some(0), b"bad!")?;
    drop(loader);
    root.remove("loader-test-entry", NodeKind::File, scratch)?;
    let replacement =
        root.create_file("loader-test-entry", 0o755, domain, Ok::<FileObject, Error>)?;
    replacement.write(Some(0), b"not a loader")?;
    builder
        .object()
        .seal()
        .map_err(|_| TestError::Contract("captured loader survives source replacement"))?;
    builder
        .object()
        .abort()
        .map_err(|_| TestError::Contract("sealed loader rollback"))?;

    let rejected = ProcessBuilder::try_publication(
        &factory,
        group.object(),
        sponsor.object(),
        prepare_file(root, domain)?,
        &replacement,
    )
    .map_err(|_| TestError::Contract("loader validation is deferred until seal"))?;
    rejected
        .object()
        .set_name("rejected-loader")
        .map_err(|_| TestError::Contract("rejected loader name"))?;
    replacement.write(Some(0), &bytes)?;
    drop(replacement);
    drop(main);
    root.remove("loader-test-entry", NodeKind::File, scratch)?;
    root.remove("loader-test-main", NodeKind::File, scratch)?;
    check(
        matches!(
            rejected.object().seal(),
            Err(ProcessBuilderError::Image(LoaderError::Elf(_)))
        ),
        "repairing the source cannot replace a captured malformed loader",
    )?;
    rejected
        .object()
        .set_name("still-open")
        .map_err(|_| TestError::Contract("failed seal preserves open builder"))?;
    rejected
        .object()
        .abort()
        .map_err(|_| TestError::Contract("failed loader rollback"))?;
    Ok(())
}

fn prepare_file(
    root: &DirectoryObject,
    domain: &ResourceDomain,
) -> Result<crate::kernel::capability::PreparedHandle, TestError> {
    use crate::kernel::authority::Rights;
    use crate::kernel::capability::{HandleFlags, PreparedHandle};
    use crate::kernel::object::ObjectPublication;
    let file = root.open_file("loader-test-main", domain)?;
    let publication = ObjectPublication::try_new(file)
        .map_err(|_| TestError::Contract("loader test file publication"))?;
    PreparedHandle::try_from_new_object(
        publication,
        Rights::READ.union(Rights::EXECUTE).union(Rights::PROTECT),
        HandleFlags::NONE,
    )
    .map_err(|_| TestError::Contract("loader test file authority"))
}

fn loader_image() -> Result<Vec<u8>, TestError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(8192)
        .map_err(|_| Error::Allocation)?;
    bytes.resize(8192, 0);
    bytes[..8].copy_from_slice(b"\x7fELF\x02\x01\x01\x3f");
    let (machine, flags) = match crate::hal::user::host_machine() {
        crate::hal::user::HostMachine::Aarch64 => (183, 0),
        crate::hal::user::HostMachine::Riscv64 => (243, 4),
        _ => return Err(TestError::Contract("loader test machine")),
    };
    for (offset, value, width) in [
        (16, 3_u64, 2),
        (18, machine, 2),
        (20, 1, 4),
        (24, 4096, 8),
        (32, 64, 8),
        (48, flags, 4),
        (52, 64, 2),
        (54, 56, 2),
        (56, 1, 2),
        (64, 1, 4), // One read/execute LOAD, including the ELF header.
        (68, 5, 4),
        (96, 8192, 8),
        (104, 8192, 8),
        (112, 4096, 8),
    ] {
        bytes[offset..offset + width].copy_from_slice(&value.to_le_bytes()[..width]);
    }
    Ok(bytes)
}
