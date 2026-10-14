// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Real namespace replacement: retained capability identity and concurrent lookup.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use hyper::fs::NodeKind;
use hyper::mm::FallibleArc;
use hyper::sync::SpinLock;

use super::{TestError, check};
use crate::kernel::accounting::{ResourceDomain, ResourceKind, ResourceLimits};
use crate::kernel::vfs::{DirectoryObject, Error, FileObject, ScratchBudget};

#[path = "root_promotion.rs"]
mod promotion;

pub(super) fn run(domain: &ResourceDomain) -> Result<(), TestError> {
    promotion::run(domain)?;
    reversible_root(domain)?;
    let root = new_namespace(domain)?;
    let scratch = ScratchBudget::new(domain);
    for path in [".next", "data", "system", "ordinary"] {
        root.create(path, NodeKind::Directory, 0o755, &scratch)?;
    }
    root.create_file("ordinary/file", 0o644, domain, Ok::<FileObject, Error>)?;
    root.create_file("system/tool", 0o644, domain, Ok::<FileObject, Error>)?;
    root.mount_ramfs("data", domain)?;
    let data = root.create_file("data/payload", 0o644, domain, Ok::<FileObject, Error>)?;
    data.write(Some(0), b"retained")?;
    let identity = data.info()?.location;
    let old_root = root.info()?.location;
    let fixed = root.open_directory("/", domain)?;
    let ordinary = root.open_directory("ordinary", domain)?;
    let cwd = DirectoryObject::scope(&root, &ordinary, domain)?;
    let following = DirectoryObject::scope(&root, &root, domain)?;
    check(
        fixed.mount_ramfs("ordinary", domain) == Err(Error::AccessDenied),
        "fixed root cannot administer namespace",
    )?;
    root.mount_ramfs(".next", domain)?;
    for path in [".next/old", ".next/data", ".next/system"] {
        root.create(path, NodeKind::Directory, 0o755, &scratch)?;
    }
    root.create(".next/old/hidden", NodeKind::Directory, 0o755, &scratch)?;
    check(
        root.switch_root(".next", "old", &[("data", "old/hidden")], domain)
            == Err(Error::InvalidInput)
            && root.info()?.location == old_root,
        "ancestor destination cannot hide a relocated mount",
    )?;
    root.bind_ramfs("system", ".next/system", domain)?;
    let bound_tool = root.metadata(".next/system/tool", true, &scratch)?.location;
    let original_tool = root.metadata("system/tool", true, &scratch)?.location;
    check(
        bound_tool.filesystem_id == original_tool.filesystem_id
            && bound_tool.node_id == original_tool.node_id,
        "ramfs bind preserves storage identity",
    )?;
    let limited = domain
        .try_new_child(ResourceLimits::UNLIMITED.with(ResourceKind::KernelMemoryBytes, 0))
        .map_err(Error::Resource)?;
    check(
        root.switch_root(".next", "old", &[("data", "data")], &limited)
            .is_err()
            && root.info()?.location == old_root,
        "failed preparation leaves root unchanged",
    )?;
    check(
        root.switch_root(".next", "old", &[("data", "data"), ("data", "old")], domain)
            .is_err()
            && root.info()?.location == old_root,
        "duplicate relocation rejects complete transaction",
    )?;
    root.switch_root(".next", "old", &[("data", "data")], domain)?;
    check(
        root.info()?.location != old_root && following.info()?.location == root.info()?.location,
        "namespace root and normalized root follow replacement",
    )?;
    check(
        fixed.info()?.location == old_root && fixed.canonicalize("..", &scratch)? == "/",
        "opened root remains a confined fixed capability",
    )?;
    check(
        cwd.canonicalize(".", &scratch)? == "/old/ordinary"
            && cwd.metadata("file", true, &scratch).is_ok(),
        "cwd retains identity in moved old root",
    )?;
    check(
        root.metadata("data/payload", true, &scratch)?.location == identity
            && data.info()?.location == identity,
        "move preserves opened file and mount identity",
    )?;
    check(
        root.metadata("system/tool", true, &scratch)?.location == bound_tool,
        "new root retains prepared child mounts",
    )?;
    let mut bytes = [0; 8];
    check(
        data.read(0, &mut bytes)? == 8 && &bytes == b"retained",
        "opened file survives root replacement",
    )?;
    // Move back and exercise repeated publication against a real reader thread.
    root.switch_root("old", ".next", &[("data", "data")], domain)?;
    concurrent_lookup(root, domain, identity.node_id)?;
    crate::pr_info!(
        "HypeR test: VFS root replacement preserves capabilities and concurrent lookup"
    );
    Ok(())
}

/// Abort owns the original topology rather than reconstructing an inverse
/// switch. Mount identities and child edges therefore survive owner loss.
fn reversible_root(domain: &ResourceDomain) -> Result<(), TestError> {
    let root = new_namespace(domain)?;
    let scratch = ScratchBudget::new(domain);
    root.create("next", NodeKind::Directory, 0o755, &scratch)?;
    root.mount_ramfs("next", domain)?;
    for path in ["next/old", "next/tmp", "next/spare"] {
        root.create(path, NodeKind::Directory, 0o755, &scratch)?;
    }
    root.mount_ramfs("next/tmp", domain)?;
    let original = root.info()?.location;
    let target = root.metadata("next", true, &scratch)?.location;
    let child = root.metadata("next/tmp", true, &scratch)?.location;
    let token = root.prepare_root_transition("next", "old", &[], domain)?;
    check(
        root.info()?.location == original,
        "preparation does not publish root",
    )?;
    token.activate()?;
    check(
        root.info()?.location == target && root.metadata("tmp", true, &scratch)?.location == child,
        "provisional switch retains mounted child identity",
    )?;
    check(
        root.mount_ramfs("spare", domain) == Err(Error::Busy),
        "provisional root blocks competing mount publication",
    )?;
    check(
        root.unmount("tmp", domain) == Err(Error::Busy),
        "provisional root blocks competing detach",
    )?;
    token.finish(false)?;
    check(
        root.info()?.location == original
            && root.metadata("next/tmp", true, &scratch)?.location == child,
        "abort restores original topology and child identity",
    )?;
    check(
        token.finish(true) == Err(Error::InvalidInput),
        "root transition finishes once",
    )?;
    {
        let token = root.prepare_root_transition("next", "old", &[], domain)?;
        token.activate()?;
    }
    check(
        root.info()?.location == original,
        "uncommitted owner drop restores root",
    )?;
    let token = root.prepare_root_transition("next", "old", &[], domain)?;
    token.activate()?;
    token.finish(true)?;
    drop(token);
    check(
        root.info()?.location == target,
        "committed token drop retains new root",
    )?;
    root.mount_ramfs("spare", domain)?;
    check(
        root.unmount("/", domain) == Err(Error::InvalidInput),
        "active root cannot detach",
    )?;
    Ok(())
}

fn new_namespace(domain: &ResourceDomain) -> Result<DirectoryObject, TestError> {
    let filesystem = crate::kernel::vfs::instance::FilesystemInstance::try_empty_ramfs(domain)?;
    let cache =
        crate::kernel::io_cache::FileDataCache::try_new_system().map_err(|_| Error::Allocation)?;
    let namespace = crate::kernel::vfs::instance::MountNamespace::try_new(
        filesystem,
        FallibleArc::try_new(cache).map_err(|_| Error::Allocation)?,
    )?;
    Ok(DirectoryObject::try_root(namespace, domain)?)
}

struct Reader {
    root: DirectoryObject,
    domain: ResourceDomain,
    expected_node: u64,
    stop: AtomicBool,
    done: AtomicBool,
    failed: AtomicBool,
    observations: AtomicUsize,
}
static READER: SpinLock<Option<FallibleArc<Reader>>> = SpinLock::new(None);

fn concurrent_lookup(
    root: DirectoryObject,
    domain: &ResourceDomain,
    expected_node: u64,
) -> Result<(), TestError> {
    let shared = FallibleArc::try_new(Reader {
        root,
        domain: domain.clone(),
        expected_node,
        stop: AtomicBool::new(false),
        done: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        observations: AtomicUsize::new(0),
    })
    .map_err(|_| Error::Allocation)?;
    READER.with(|slot| *slot = Some(shared.clone()));
    let worker = crate::kernel::task::scheduler::kthread_create("vfs-root-reader", reader, 0)
        .map_err(|_| TestError::Contract("root reader construction"))?;
    crate::kernel::task::scheduler::thread_ready(worker)
        .map_err(|_| TestError::Contract("root reader publication"))?;
    let result = (|| {
        wait(|| shared.observations.load(Ordering::Acquire) >= 8)?;
        for _ in 0..32 {
            shared
                .root
                .switch_root(".next", "old", &[("data", "data")], domain)?;
            crate::kernel::task::scheduler::yield_now()
                .map_err(|_| TestError::Contract("root reader yield"))?;
            shared
                .root
                .switch_root("old", ".next", &[("data", "data")], domain)?;
        }
        check(
            !shared.failed.load(Ordering::Acquire),
            "lookup never sees half-published mount topology",
        )
    })();
    shared.stop.store(true, Ordering::Release);
    wait(|| shared.done.load(Ordering::Acquire))?;
    let retained = READER.with(Option::take);
    drop(retained);
    result?;
    check(
        !shared.failed.load(Ordering::Acquire),
        "reader finishes without namespace corruption",
    )
}

fn wait(predicate: impl Fn() -> bool) -> Result<(), TestError> {
    let done = crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, crate::kernel::task::SleepError>(predicate()),
    )
    .map_err(|_| TestError::Contract("root reader wait"))?;
    check(done, "namespace reader makes progress")
}

extern "C" fn reader(_: usize) {
    let Some(shared) = READER.with(|slot| slot.clone()) else {
        return;
    };
    let scratch = ScratchBudget::new(&shared.domain);
    while !shared.stop.load(Ordering::Acquire) {
        match shared.root.metadata("/data/payload", true, &scratch) {
            Ok(metadata) if metadata.location.node_id == shared.expected_node => {
                shared.observations.fetch_add(1, Ordering::Release);
            }
            Err(Error::Busy) => {}
            _ => {
                shared.failed.store(true, Ordering::Release);
                break;
            }
        }
        if crate::kernel::task::scheduler::yield_now().is_err() {
            shared.failed.store(true, Ordering::Release);
            break;
        }
    }
    shared.done.store(true, Ordering::Release);
}
