// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real ramfs namespace and publication contracts in scheduled kernel context.

#[path = "../../../tests/kernel/vfs_namespace/directory.rs"]
mod directory;

#[path = "../../../tests/kernel/vfs_namespace/content.rs"]
mod content;

#[path = "../../../tests/kernel/vfs_namespace/protection.rs"]
mod protection;

#[path = "../../../tests/kernel/vfs_namespace/root_switch.rs"]
mod root_switch;

#[path = "../../../tests/kernel/vfs_namespace/archive_lifetime.rs"]
mod archive_lifetime;

#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
#[path = "../../../tests/kernel/vfs_namespace/process_loader.rs"]
mod process_loader;

static EMPTY_ARCHIVE: &[u8] = b"07070100000001000000000000000000000000000000010000000000000000000000000000000000000000000000000000000b00000000TRAILER!!!\0\0\0\0";

use super::{DirectoryObject, Error, FileObject, FileOpenOptions, MetadataUpdate, ScratchBudget};
use crate::authority::Rights;
use hyper::mm::FallibleArc;
use {crate::accounting::ResourceDomain, crate::accounting::ResourceLimits};
use {crate::fs::primitives::NodeKind, crate::fs::primitives::ramfs::RamFs};

#[derive(Debug)]
pub(crate) enum TestError {
    Vfs(Error),
    Contract(&'static str),
}
impl From<Error> for TestError {
    fn from(error: Error) -> Self {
        Self::Vfs(error)
    }
}
impl From<super::instance::Error> for TestError {
    fn from(error: super::instance::Error) -> Self {
        Self::Vfs(error.into())
    }
}
fn check(condition: bool, label: &'static str) -> Result<(), TestError> {
    if condition {
        Ok(())
    } else {
        Err(TestError::Contract(label))
    }
}

pub(crate) fn run() -> Result<(), TestError> {
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(Error::Resource)?;
    super::remote::test_record_storage(&domain)?;
    archive_lifetime::run(&domain)?;
    let scratch = ScratchBudget::new(&domain);
    let archive = RamFs::from_newc(EMPTY_ARCHIVE)
        .map_err(|error| Error::Backend(super::instance::Error::RamFs(error)))?;
    let filesystem =
        super::instance::FilesystemInstance::try_from_ramfs(archive).map_err(Error::from)?;
    let cache = crate::fs::cache::FileDataCache::try_new_system().map_err(|_| Error::Allocation)?;
    let cache = FallibleArc::try_new(cache).map_err(|_| Error::Allocation)?;
    let namespace =
        super::instance::MountNamespace::try_new(filesystem, cache).map_err(Error::from)?;
    let root = DirectoryObject::try_root(namespace, &domain)?;
    directory::run(&root, &domain, &scratch)?;
    content::run(&root, &domain, &scratch)?;
    protection::run(&root, &domain, &scratch)?;
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    process_loader::run(&root, &domain, &scratch)?;
    mounted_namespace(&root, &domain, &scratch)?;
    root.create("left", NodeKind::Directory, 0o755, &scratch)?;
    root.create("right", NodeKind::Directory, 0o755, &scratch)?;
    let file = root.create_file("left/item", 0o666, &domain, Ok::<FileObject, Error>)?;
    file.write(Some(0), b"retained")?;
    let identity = file.metadata()?.location.node_id;
    root.link("left/item", &root, "other", &scratch)?;
    root.rename("left/item", &root, "other", &scratch)?;
    check(
        root.metadata("left/item", false, &scratch)?
            .location
            .node_id
            == identity
            && root.metadata("other", false, &scratch)?.location.node_id == identity,
        "rename aliases of one node is a no-op",
    )?;
    root.rename("left/item", &root, "right/moved", &scratch)?;
    check(
        root.metadata("other", true, &scratch)?.location.node_id == identity,
        "hardlink identity",
    )?;
    let left = root.open_directory("left", &domain)?;
    let cwd = DirectoryObject::scope(&root, &left, &domain)?;
    root.rename("left", &root, "right/inside", &scratch)?;
    check(
        cwd.canonicalize(".", &scratch)? == "/right/inside",
        "cwd follows rename",
    )?;
    check(
        cwd.metadata("../moved", true, &scratch)?.location.node_id == identity,
        "rooted parent traversal",
    )?;
    check(
        root.rename("right", &root, "right/inside/loop", &scratch) == Err(Error::InvalidInput),
        "directory cycle rejection",
    )?;
    root.symlink("right/inside/absolute", "/other", &scratch)?;
    check(
        cwd.metadata("absolute", true, &scratch)?.location.node_id == identity,
        "rooted absolute symlink",
    )?;
    check(
        left.metadata("absolute", true, &scratch) == Err(Error::Missing),
        "confined absolute symlink",
    )?;
    check(
        root.metadata("right/inside/absolute", false, &scratch)?
            .attributes
            .kind()
            == NodeKind::Symlink,
        "nofollow metadata",
    )?;
    root.symlink("loop", "loop", &scratch)?;
    check(
        root.metadata("loop", true, &scratch) == Err(Error::SymlinkLoop),
        "symlink loop classification",
    )?;
    check(
        &root.read_link("loop", &scratch)?[..] == b"loop",
        "readlink preserves target",
    )?;
    root.symlink("dangling", "right/new", &scratch)?;
    let created = root.open_file_with_options(
        "dangling",
        &FileOpenOptions::new(Rights::READ.union(Rights::WRITE), 1, 0o666)?,
        &domain,
        Ok::<FileObject, Error>,
    )?;
    check(
        root.metadata("right/new", true, &scratch)?.location.node_id
            == created.metadata()?.location.node_id,
        "dangling symlink creation",
    )?;
    check(
        matches!(
            root.open_file_with_options(
                "dangling",
                &FileOpenOptions::new(Rights::WRITE, 3, 0o666)?,
                &domain,
                Ok::<FileObject, Error>
            ),
            Err(Error::AlreadyExists)
        ),
        "exclusive create does not follow final symlink",
    )?;
    let read_only_created = root.open_file_with_options(
        "read-only-created",
        &FileOpenOptions::new(Rights::READ, 3, 0o600)?,
        &domain,
        Ok::<FileObject, Error>,
    )?;
    check(
        read_only_created.metadata()?.attributes.mode() == 0o600,
        "create with read-only result",
    )?;
    for bits in [2, 8, 4] {
        check(
            matches!(
                FileOpenOptions::new(Rights::READ, bits, 0o600),
                Err(Error::InvalidInput)
            ),
            "exclusive needs create; unknown options and read-only truncate fail",
        )?;
    }
    check(
        root.remove_if("other", false, identity + 1, &scratch) == Err(Error::Busy),
        "conditional removal identity",
    )?;
    root.remove_if("other", false, identity, &scratch)?;
    root.remove("right/moved", NodeKind::File, &scratch)?;
    let mut bytes = [0; 8];
    check(
        file.read(0, &mut bytes)? == 8 && &bytes == b"retained",
        "open lease survives final unlink",
    )?;
    check(
        root.metadata("right/new/..", true, &scratch) == Err(Error::NotDirectory),
        "regular file cannot be traversed",
    )?;
    check(
        root.metadata("right/new/", true, &scratch) == Err(Error::NotDirectory),
        "trailing slash requires directory",
    )?;
    check(
        root.canonicalize("//right///inside/.", &scratch)? == "/right/inside",
        "separator normalization",
    )?;
    created.write(Some(0), b"before")?;
    let failed: Result<(), Error> = root.open_file_with_options(
        "right/new",
        &FileOpenOptions::new(Rights::WRITE, 4, 0o666)?,
        &domain,
        |_| Err(Error::InvalidInput),
    );
    check(
        failed == Err(Error::InvalidInput) && created.len()? == 6,
        "publication failure cannot truncate",
    )?;
    let failed: Result<(), Error> =
        root.create_file("failed", 0o666, &domain, |_| Err(Error::InvalidInput));
    check(
        failed == Err(Error::InvalidInput)
            && root.metadata("failed", false, &scratch) == Err(Error::Missing),
        "publication failure cannot create name",
    )?;
    created.set_metadata(MetadataUpdate {
        mode: Some(0o444),
        ..MetadataUpdate::default()
    })?;
    let readonly = root.open_file("right/new", &domain)?;
    check(
        readonly.validate_access(Rights::WRITE) == Err(Error::AccessDenied),
        "new write admission honors mode",
    )?;
    check(
        created.write(Some(0), b"after!")?.0 == 6,
        "metadata does not revoke existing authority",
    )?;
    readonly.set_metadata(MetadataUpdate {
        mode: Some(0o666),
        ..MetadataUpdate::default()
    })?;
    check(
        root.open_file("right/new", &domain)?
            .validate_access(Rights::WRITE)
            .is_ok(),
        "metadata mutation independent of write",
    )?;
    let displaced = root.create_file("target", 0o666, &domain, Ok::<FileObject, Error>)?;
    displaced.write(Some(0), b"old")?;
    let replacement = root.create_file("source", 0o666, &domain, Ok::<FileObject, Error>)?;
    replacement.write(Some(0), b"new")?;
    root.rename("source", &root, "target", &scratch)?;
    let mut previous = [0; 3];
    check(
        displaced.read(0, &mut previous)? == 3 && &previous == b"old",
        "rename replacement preserves open target lease",
    )?;
    check(
        root.metadata("target", true, &scratch)?.location.node_id
            == replacement.metadata()?.location.node_id,
        "rename target atomically changes identity",
    )?;
    request_budget_follows_caller(&root)?;
    trailing_directory_mutations(&root, &domain, &scratch)?;
    deep_directory_rename(&root, &domain, &scratch)?;
    active_handle_lock_lifetime(&root, &domain)?;
    root_switch::run(&domain)?;
    Ok(())
}

fn active_handle_lock_lifetime(
    root: &DirectoryObject,
    domain: &ResourceDomain,
) -> Result<(), TestError> {
    use super::locks::{LockError, LockMode};
    use crate::object::ObjectPublication;
    let first = root.open_file("right/new", domain)?;
    let second = root.open_file("right/new", domain)?;
    let active = ObjectPublication::try_new(first)
        .map_err(Error::Object)?
        .activate()
        .map_err(|_| TestError::Contract("activate file"))?;
    let pin = active
        .pin::<FileObject>()
        .ok_or(TestError::Contract("pin file"))?;
    let duplicate = active
        .try_duplicate()
        .map_err(|_| TestError::Contract("duplicate file"))?;
    pin.object()
        .lock(LockMode::Exclusive, 0, domain, || false)
        .map_err(|_| TestError::Contract("acquire first lock"))?;
    drop(active);
    check(
        matches!(
            second.lock(LockMode::Exclusive, 0, domain, || false),
            Err(LockError::WouldBlock)
        ),
        "duplicate retains advisory lock",
    )?;
    drop(duplicate);
    second
        .lock(LockMode::Exclusive, 0, domain, || false)
        .map_err(|_| TestError::Contract("last close releases advisory lock"))?;
    check(
        matches!(
            pin.object().lock(LockMode::Exclusive, 0, domain, || false),
            Err(LockError::Closed)
        ),
        "operation pin cannot resurrect closed lock owner",
    )?;
    second
        .unlock()
        .map_err(|_| TestError::Contract("unlock second file"))?;
    Ok(())
}

fn request_budget_follows_caller(root: &DirectoryObject) -> Result<(), TestError> {
    use crate::accounting::ResourceKind;
    // Earlier cases release published objects asynchronously. Their shared
    // domain can shrink during these requests, independently of scratch use.
    let creator =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(Error::Resource)?;
    let root = DirectoryObject::scope(root, root, &creator)?;
    let creator_before = creator.usage();
    // No creator headroom: even a temporary charge released before the final
    // snapshot must fail instead of hiding incorrect request sponsorship.
    creator
        .set_local_limits(ResourceLimits::UNLIMITED.with(
            ResourceKind::KernelMemoryBytes,
            creator_before.total(ResourceKind::KernelMemoryBytes),
        ))
        .map_err(Error::Resource)?;
    let caller = ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED.with(ResourceKind::KernelMemoryBytes, 4096),
    )
    .map_err(Error::Resource)?;
    let budget = ScratchBudget::new(&caller);
    check(
        root.metadata("right/new", true, &budget)?.attributes.kind() == NodeKind::File,
        "small caller quota supports metadata",
    )?;
    check(
        caller.usage().total(ResourceKind::KernelMemoryBytes) == 0,
        "metadata scratch releases on return",
    )?;
    let canonical = root.canonicalize("right/new", &budget)?;
    check(
        canonical == "/right/new",
        "small caller quota supports canonicalize",
    )?;
    check(
        caller.usage().total(ResourceKind::KernelMemoryBytes) == canonical.capacity() as u64,
        "returned path retains exact caller charge",
    )?;
    check(
        creator.usage() == creator_before,
        "transferred directory never charges creator for request",
    )?;
    drop(canonical);
    check(
        caller.usage().total(ResourceKind::KernelMemoryBytes) == 0,
        "returned path drop releases caller charge",
    )?;
    let constrained = ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED.with(ResourceKind::KernelMemoryBytes, 16),
    )
    .map_err(Error::Resource)?;
    let constrained_budget = ScratchBudget::new(&constrained);
    check(
        matches!(
            root.canonicalize("right/new", &constrained_budget),
            Err(Error::Resource(_))
        ),
        "scratch quota enforced before growth",
    )?;
    check(
        constrained.usage().total(ResourceKind::KernelMemoryBytes) == 0,
        "failed request releases partial scratch",
    )?;
    check(
        creator.usage() == creator_before,
        "failed caller request preserves creator accounting",
    )?;
    Ok(())
}

fn trailing_directory_mutations(
    root: &DirectoryObject,
    domain: &ResourceDomain,
    scratch: &ScratchBudget,
) -> Result<(), TestError> {
    root.create("trailing/", NodeKind::Directory, 0o755, scratch)?;
    root.rename("trailing/", root, "renamed-trailing/", scratch)?;
    root.symlink("trailing-link", "renamed-trailing", scratch)?;
    check(
        root.remove("trailing-link/", NodeKind::Directory, scratch) == Err(Error::NotDirectory),
        "trailing slash never follows removal symlink",
    )?;
    check(
        root.rename("trailing-link/", root, "trailing-link/", scratch) == Err(Error::NotDirectory),
        "directory requirement precedes same-node rename no-op",
    )?;
    check(
        root.rename("renamed-trailing", root, "trailing-link/", scratch)
            == Err(Error::NotDirectory),
        "rename cannot replace directory-required symlink",
    )?;
    check(
        root.metadata("trailing-link", false, scratch)?
            .attributes
            .kind()
            == NodeKind::Symlink
            && root
                .metadata("renamed-trailing", false, scratch)?
                .attributes
                .kind()
                == NodeKind::Directory,
        "failed trailing symlink mutations preserve link and target",
    )?;
    check(
        root.remove("right/new/", NodeKind::File, scratch) == Err(Error::NotDirectory),
        "trailing slash cannot unlink regular file",
    )?;
    check(
        root.rename("right/new/", root, "bad-rename", scratch) == Err(Error::NotDirectory),
        "rename source directory requirement",
    )?;
    check(
        root.rename("right/new", root, "bad-rename/", scratch) == Err(Error::NotDirectory),
        "rename destination directory requirement",
    )?;
    check(
        matches!(
            root.create_file("bad-file/", 0o666, domain, Ok::<FileObject, Error>),
            Err(Error::NotDirectory)
        ),
        "trailing slash cannot create regular file",
    )?;
    check(
        root.metadata("bad-file", false, scratch) == Err(Error::Missing),
        "failed file directory requirement leaves no name",
    )?;
    check(
        root.link("right/new", root, "bad-link/", scratch) == Err(Error::NotDirectory),
        "hardlink destination directory requirement",
    )?;
    check(
        root.metadata("bad-link", false, scratch) == Err(Error::Missing),
        "failed link directory requirement leaves no name",
    )?;
    root.remove("renamed-trailing/", NodeKind::Directory, scratch)?;
    root.remove("trailing-link", NodeKind::File, scratch)?;
    Ok(())
}

fn deep_directory_rename(
    root: &DirectoryObject,
    domain: &ResourceDomain,
    scratch: &ScratchBudget,
) -> Result<(), TestError> {
    root.create("deep", NodeKind::Directory, 0o755, scratch)?;
    let mut ancestors = super::ScratchVec::new(scratch.clone());
    ancestors
        .push(root.open_directory("deep", domain)?)
        .map_err(Error::from)?;
    let result = (|| -> Result<(), TestError> {
        for _ in 0..260 {
            let current = ancestors.last().ok_or(TestError::Contract("deep parent"))?;
            current.create("d", NodeKind::Directory, 0o755, scratch)?;
            let child = current.open_directory("d", domain)?;
            ancestors.push(child).map_err(Error::from)?;
        }
        let current = ancestors.last().ok_or(TestError::Contract("deep leaf"))?;
        current.create("first", NodeKind::Directory, 0o755, scratch)?;
        current.create("second", NodeKind::Directory, 0o755, scratch)?;
        current.rename("first", current, "renamed", scratch)?;
        let second = current.open_directory("second", domain)?;
        current.rename("renamed", &second, "moved", scratch)?;
        let moved = second.open_directory("moved", domain)?;
        check(
            second.rename("moved", &moved, "cycle", scratch) == Err(Error::InvalidInput),
            "deep directory cycle remains rejected",
        )?;
        Ok(())
    })();
    // Explicit bottom-up cleanup also runs after a failed assertion. A deep
    // test fixture must never depend on recursive tree destruction at teardown.
    let mut cleanup = Ok(());
    while let Some(directory) = ancestors.pop() {
        for name in ["second/moved", "second", "renamed", "first", "d"] {
            match directory.remove(name, NodeKind::Directory, scratch) {
                Ok(()) | Err(Error::Missing) => {}
                Err(error) => {
                    if cleanup.is_ok() {
                        cleanup = Err(TestError::Vfs(error));
                    }
                }
            }
        }
    }
    match root.remove("deep", NodeKind::Directory, scratch) {
        Ok(()) | Err(Error::Missing) => {}
        Err(error) => {
            if cleanup.is_ok() {
                cleanup = Err(TestError::Vfs(error));
            }
        }
    }
    result.and(cleanup)
}

fn mounted_namespace(
    root: &DirectoryObject,
    domain: &ResourceDomain,
    scratch: &ScratchBudget,
) -> Result<(), TestError> {
    root.create("data", NodeKind::Directory, 0o755, scratch)?;
    root.create_file("data/covered", 0o644, domain, Ok::<FileObject, Error>)?;
    let covered = root.open_directory("data", domain)?;
    let old_cwd = DirectoryObject::scope(root, &covered, domain)?;
    let archive = RamFs::from_newc(EMPTY_ARCHIVE)
        .map_err(|error| Error::Backend(super::instance::Error::RamFs(error)))?;
    let filesystem =
        super::instance::FilesystemInstance::try_from_ramfs(archive).map_err(Error::from)?;
    root.mount_filesystem("data", filesystem, domain)?;
    check(
        matches!(
            root.metadata("data/covered", true, scratch),
            Err(Error::Missing)
        ),
        "mount covers, rather than copies, the ramfs directory",
    )?;
    check(
        covered.metadata("covered", true, scratch).is_ok(),
        "existing directory capability retains the covered mount",
    )?;
    root.create_file("data/on-disk", 0o644, domain, Ok::<FileObject, Error>)?;
    check(
        old_cwd.metadata("covered", true, scratch).is_ok(),
        "scoped pre-mount cwd retains covered directory",
    )?;
    check(
        matches!(
            old_cwd.metadata("on-disk", true, scratch),
            Err(Error::Missing)
        ),
        "old cwd does not silently gain mounted children",
    )?;
    check(
        old_cwd.metadata("../data/on-disk", true, scratch).is_ok(),
        "old cwd parent then re-entry follows current mount",
    )?;
    check(
        old_cwd.metadata("/data/on-disk", true, scratch).is_ok(),
        "absolute lookup from scoped cwd follows namespace mount",
    )?;
    let data = root.open_directory("data", domain)?;
    let cwd = DirectoryObject::scope(root, &data, domain)?;
    check(
        cwd.canonicalize(".", scratch)? == "/data",
        "mount ancestry from cwd",
    )?;
    check(
        cwd.canonicalize("..", scratch)? == "/",
        "parent exits mount",
    )?;
    let confined = DirectoryObject::scope(&data, &data, domain)?;
    check(
        confined.canonicalize("..", scratch)? == "/",
        "capability root confines mount parent",
    )?;
    check(
        matches!(
            root.remove("data", NodeKind::Directory, scratch),
            Err(Error::Busy)
        ),
        "mounted directory cannot be removed",
    )?;
    check(
        matches!(
            root.rename("data", root, "moved-data", scratch),
            Err(Error::Busy)
        ),
        "mounted directory cannot be renamed",
    )?;
    let file = root.open_file("data/on-disk", domain)?;
    file.write(Some(0), b"mounted")?;
    let mut buffer = [0; 7];
    check(
        file.read(0, &mut buffer)? == 7 && &buffer == b"mounted",
        "mounted file IO",
    )?;
    let old_mount = file.metadata()?.location.mount_id;
    check(
        matches!(root.unmount("data", domain), Err(Error::Busy)),
        "open file and cwd block unmount",
    )?;
    drop(file);
    drop(confined);
    drop(cwd);
    drop(data);
    root.unmount("data", domain)?;
    check(
        root.metadata("data/covered", true, scratch).is_ok(),
        "unmount uncovers original directory",
    )?;
    let archive = RamFs::from_newc(EMPTY_ARCHIVE)
        .map_err(|error| Error::Backend(super::instance::Error::RamFs(error)))?;
    let filesystem = super::instance::FilesystemInstance::try_from_ramfs(archive)?;
    root.mount_filesystem("data", filesystem, domain)?;
    check(
        root.metadata("data", true, scratch)?.location.mount_id != old_mount,
        "remount has a fresh mount identity",
    )?;
    root.unmount("data", domain)?;
    Ok(())
}
