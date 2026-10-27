// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! A RAM subtree becomes a mount without retaining its old directory name.

use super::{DirectoryObject, Error, FileObject, ResourceDomain, ResourceKind, ResourceLimits};
use super::{NodeKind, ScratchBudget, TestError, check, new_namespace};

pub(super) fn run(domain: &ResourceDomain) -> Result<(), TestError> {
    maximum_sources(domain)?;
    shared_filesystem_root(domain)?;
    let root = new_namespace(domain)?;
    let scratch = ScratchBudget::new(domain);
    for path in [
        ".next",
        "system",
        "system/child",
        "sibling",
        "arena",
        "alias",
    ] {
        root.create(path, NodeKind::Directory, 0o755, &scratch)?;
    }
    let file = root.create_file("system/tool", 0o644, domain, Ok::<FileObject, Error>)?;
    file.write(Some(0), b"resident")?;
    let identity = file.info()?.location;
    let source = root.open_directory("system", domain)?;
    let subtree = DirectoryObject::scope(&source, &source, domain)?;
    let cwd = DirectoryObject::scope(&root, &source, domain)?;
    let old_root = root.info()?.location;
    root.mount_ramfs("arena", domain)?;
    root.create("arena/lib", NodeKind::Directory, 0o755, &scratch)?;
    root.create_file("arena/lib/runtime", 0o644, domain, Ok::<FileObject, Error>)?;
    root.mount_ramfs(".next", domain)?;
    for path in [".next/boot", ".next/system", ".next/sibling", ".next/lib"] {
        root.create(path, NodeKind::Directory, 0o755, &scratch)?;
    }
    let moves = [
        ("system", "system"),
        ("sibling", "sibling"),
        ("arena/lib", "lib"),
    ];
    let limited = domain
        .try_new_child(ResourceLimits::UNLIMITED.with(ResourceKind::KernelMemoryBytes, 0))
        .map_err(Error::Resource)?;
    check(
        root.switch_root(".next", "boot", &moves, &limited).is_err()
            && root.metadata("system/tool", true, &scratch)?.location == identity
            && root.info()?.location == old_root,
        "failed promotion preparation leaves source names and root intact",
    )?;
    check(
        root.switch_root(
            ".next",
            "boot",
            &[("system", "system"), ("sibling", "system")],
            domain,
        ) == Err(Error::InvalidInput)
            && root.metadata("system/tool", true, &scratch)?.location == identity,
        "duplicate promotion destinations preserve source dirents",
    )?;
    check(
        root.switch_root(
            ".next",
            "boot",
            &[("system", "system"), ("system/child", "sibling")],
            domain,
        ) == Err(Error::InvalidInput),
        "overlapping RAM sources reject the transaction",
    )?;
    root.mount_ramfs("system/child", domain)?;
    check(
        root.switch_root(".next", "boot", &moves, domain) == Err(Error::Busy)
            && root.metadata("system/tool", true, &scratch)?.location == identity,
        "RAM promotion refuses to orphan an attached child mount",
    )?;
    root.unmount("system/child", domain)?;
    root.bind_ramfs("system", "alias", domain)?;
    root.switch_root(".next", "boot", &moves, domain)?;
    for path in ["boot/system", "boot/sibling", "boot/arena/lib"] {
        check(
            matches!(root.metadata(path, true, &scratch), Err(Error::Missing)),
            "promoted directory has no stale source name",
        )?;
    }
    let moved = root.metadata("system/tool", true, &scratch)?.location;
    check(
        moved.filesystem_id == identity.filesystem_id
            && moved.node_id == identity.node_id
            && moved.mount_id != identity.mount_id
            && file.info()?.location == identity,
        "RAM promotion preserves file identity and assigns a new mount identity",
    )?;
    let mut bytes = [0; 8];
    check(
        file.read(0, &mut bytes)? == 8 && &bytes == b"resident",
        "opened file survives promotion",
    )?;
    check(
        matches!(cwd.canonicalize(".", &scratch), Err(Error::Missing))
            && matches!(cwd.metadata("tool", true, &scratch), Err(Error::Missing))
            && cwd.metadata("/system/tool", true, &scratch)?.location == moved,
        "detached contextual cwd fails relative lookup but retains live-root absolute lookup",
    )?;
    check(
        subtree.canonicalize("..", &scratch)? == "/"
            && subtree.metadata("tool", true, &scratch)?.location == identity,
        "fixed subtree scope keeps identity and cannot escape its root",
    )?;
    // Recreating a scope from the retained Directory must work after detachment.
    let rescope = DirectoryObject::scope(&source, &source, domain)?;
    rescope.create("created-by-old-scope", NodeKind::Directory, 0o755, &scratch)?;
    root.create(
        "system/created-by-new-root",
        NodeKind::Directory,
        0o755,
        &scratch,
    )?;
    root.create("sibling/writable", NodeKind::Directory, 0o755, &scratch)?;
    root.create("lib/writable", NodeKind::Directory, 0o755, &scratch)?;
    check(
        root.metadata("system/created-by-old-scope", true, &scratch)
            .is_ok()
            && rescope
                .metadata("created-by-new-root", true, &scratch)
                .is_ok(),
        "both retained and mounted scopes can mutate the detached RAM root",
    )?;
    // A previously bound mount independently owns this root after the promoted
    // mount is removed; neither path may turn its writable root into a tombstone.
    root.unmount("system", domain)?;
    root.create(
        "boot/alias/still-writable",
        NodeKind::Directory,
        0o755,
        &scratch,
    )?;
    check(
        rescope.metadata("still-writable", true, &scratch).is_ok(),
        "bound alias keeps a promoted RAM root writable after unmount",
    )?;
    crate::pr_info!("HypeR test: RAM root promotion preserves identity and scope confinement");
    Ok(())
}

/// Exercise the ABI's maximum lock depth, with a different filesystem for each
/// source and for the destination root. This also covers retaining source mounts
/// below the old root while only their selected directories are promoted.
fn maximum_sources(domain: &ResourceDomain) -> Result<(), TestError> {
    use {crate::fs::vfs::ScratchString, crate::fs::vfs::ScratchVec};

    let names = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p",
    ];
    let root = new_namespace(domain)?;
    let scratch = ScratchBudget::new(domain);
    root.create(".next", NodeKind::Directory, 0o755, &scratch)?;
    root.mount_ramfs(".next", domain)?;
    root.create(".next/boot", NodeKind::Directory, 0o755, &scratch)?;
    let staging = root.open_directory(".next", domain)?;
    let mut sources = ScratchVec::new(scratch.clone());
    for name in names {
        root.create(name, NodeKind::Directory, 0o755, &scratch)?;
        root.mount_ramfs(name, domain)?;
        let mut path = ScratchString::from_str(name, scratch.clone()).map_err(Error::from)?;
        path.push_str("/source").map_err(Error::from)?;
        root.create(&path, NodeKind::Directory, 0o755, &scratch)?;
        staging.create(name, NodeKind::Directory, 0o755, &scratch)?;
        sources.push(path).map_err(Error::from)?;
    }
    let mut moves = ScratchVec::new(scratch.clone());
    for (source, target) in sources.iter().zip(names) {
        moves.push((source.as_str(), target)).map_err(Error::from)?;
    }
    root.switch_root(".next", "boot", &moves, domain)?;
    for name in names {
        check(
            root.metadata(name, true, &scratch)?.attributes.kind() == NodeKind::Directory,
            "maximum RAM promotion publishes every source",
        )?;
    }
    Ok(())
}

fn shared_filesystem_root(domain: &ResourceDomain) -> Result<(), TestError> {
    let root = new_namespace(domain)?;
    let scratch = ScratchBudget::new(domain);
    for path in ["target", "target/boot", "target/system", "stage", "system"] {
        root.create(path, NodeKind::Directory, 0o755, &scratch)?;
    }
    root.bind_ramfs("target", "stage", domain)?;
    root.switch_root("stage", "boot", &[("system", "system")], domain)?;
    check(
        matches!(
            root.metadata("boot/system", true, &scratch),
            Err(Error::Missing)
        ),
        "one filesystem shared by source and destination participates once in the transaction",
    )?;
    root.create("system/writable", NodeKind::Directory, 0o755, &scratch)?;
    Ok(())
}
