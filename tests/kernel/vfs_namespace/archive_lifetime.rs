// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Archive borrowers must survive root promotion, unlink and parser use.

use super::{DirectoryObject, Error, FallibleArc, NodeKind, RamFs, ResourceDomain};
use super::{ScratchBudget, TestError, check};
use crate::kernel::boot::Archive;
use crate::kernel::vfs::instance::{FilesystemInstance, MountNamespace};

const FIXTURE: &[u8] = concat!(
    "070701000000010000816d0000000000000000000000010000000000000008000000000000000000000000000000000000000c00000000system/tool\0\0\0resident",
    "07070100000001000000000000000000000000000000010000000000000000000000000000000000000000000000000000000b00000000TRAILER!!!\0\0\0\0",
).as_bytes();

pub(super) fn run(domain: &ResourceDomain) -> Result<(), TestError> {
    let archive =
        FallibleArc::try_new(Archive::static_fixture(FIXTURE)).map_err(|_| Error::Allocation)?;
    let lifetime = archive.downgrade();
    let parsed = RamFs::from_newc(FIXTURE)
        .map_err(|error| Error::Backend(crate::kernel::vfs::instance::Error::RamFs(error)))?;
    let filesystem = FilesystemInstance::try_from_owned_archive(parsed, archive)?;
    let cache =
        crate::kernel::io_cache::FileDataCache::try_new_system().map_err(|_| Error::Allocation)?;
    let cache = FallibleArc::try_new(cache).map_err(|_| Error::Allocation)?;
    let namespace = MountNamespace::try_new(filesystem, cache)?;
    let root = DirectoryObject::try_root(namespace, domain)?;
    let scratch = ScratchBudget::new(domain);
    root.create(".next", NodeKind::Directory, 0o755, &scratch)?;
    root.mount_ramfs(".next", domain)?;
    root.create(".next/boot", NodeKind::Directory, 0o755, &scratch)?;
    root.create(".next/system", NodeKind::Directory, 0o755, &scratch)?;
    root.switch_root(".next", "boot", &[("system", "system")], domain)?;
    root.unmount("boot", domain)?;
    check(
        lifetime.upgrade().is_some(),
        "promoted archive subtree pins its backing",
    )?;
    let file = root.open_file("system/tool", domain)?;
    let snapshot = file.executable_snapshot(domain)?;
    root.remove("system/tool", NodeKind::File, &scratch)?;
    drop(root);
    let mut bytes = [0; 8];
    check(
        file.read(0, &mut bytes)? == 8 && &bytes == b"resident",
        "unlinked archive file survives namespace destruction",
    )?;
    drop(file);
    check(
        lifetime.upgrade().is_some() && snapshot.bytes() == b"resident",
        "parser snapshot pins backing independently of the final file",
    )?;
    drop(snapshot);
    check(
        lifetime.upgrade().is_none(),
        "last archive borrower releases its backing",
    )?;
    crate::pr_info!("HypeR test: archive backing survives subtree promotion and parser borrowing");
    Ok(())
}
