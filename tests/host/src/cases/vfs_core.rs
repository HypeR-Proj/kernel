// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded VFS names, paths, and hierarchical RamFs contracts.

use hyper::fs::ramfs::{DirectoryCookie, Error as RamFsError, RamFs};
use hyper::fs::{
    MAX_NAME_BYTES, MAX_PATH_BYTES, Name, NameError, NodeKind, Path, PathComponent, PathError,
};

fn append_hex(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(format!("{value:08x}").as_bytes());
}

fn append_entry(output: &mut Vec<u8>, name: &str, mode: u32, data: &[u8]) {
    output.extend_from_slice(b"070701");
    append_hex(output, 1);
    append_hex(output, mode);
    for value in [0, 0, 1, 0] {
        append_hex(output, value);
    }
    append_hex(output, data.len() as u32);
    for value in [0, 0, 0, 0] {
        append_hex(output, value);
    }
    append_hex(output, (name.len() + 1) as u32);
    append_hex(output, 0);
    output.extend_from_slice(name.as_bytes());
    output.push(0);
    while output.len() & 3 != 0 {
        output.push(0);
    }
    output.extend_from_slice(data);
    while output.len() & 3 != 0 {
        output.push(0);
    }
}

fn archive(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
    let mut output = Vec::new();
    for (name, mode, data) in entries {
        append_entry(&mut output, name, *mode, data);
    }
    append_entry(&mut output, "TRAILER!!!", 0, &[]);
    output
}

fn name(value: &str) -> Name<'_> {
    crate::require_ok(Name::new(value))
}

#[test]
fn validates_names_without_conflating_special_path_components() {
    assert_eq!(crate::require_ok(Name::new("service")).as_str(), "service");
    assert_eq!(Name::new(""), Err(NameError::Empty));
    assert_eq!(Name::new("."), Err(NameError::Reserved));
    assert_eq!(Name::new(".."), Err(NameError::Reserved));
    assert_eq!(Name::new("a/b"), Err(NameError::ContainsSeparator));
    assert_eq!(Name::new("a\0b"), Err(NameError::ContainsNul));
    let maximum = "n".repeat(MAX_NAME_BYTES);
    assert!(Name::new(&maximum).is_ok());
    let too_long = "n".repeat(MAX_NAME_BYTES + 1);
    assert_eq!(Name::new(&too_long), Err(NameError::TooLong));
}

#[test]
fn parses_bounded_paths_into_typed_components() {
    let relative = crate::require_ok(Path::new("services/./console/../serial"));
    assert!(!relative.is_absolute());
    assert_eq!(relative.component_count(), 5);
    let components: Vec<_> = relative.components().collect();
    assert!(
        matches!(components.first(), Some(PathComponent::Name(value)) if value.as_str() == "services")
    );
    assert!(matches!(components.get(1), Some(PathComponent::Current)));
    assert!(matches!(components.get(3), Some(PathComponent::Parent)));

    let root = crate::require_ok(Path::new("/"));
    assert!(root.is_absolute());
    assert_eq!(root.as_str(), "/");
    assert_eq!(root.components().len(), 0);
    assert_eq!(Path::new(""), Err(PathError::Empty));
    assert_eq!(Path::new("a//b"), Err(PathError::EmptyComponent));
    assert_eq!(Path::new("a/"), Err(PathError::EmptyComponent));
    assert_eq!(Path::new("/a\0b"), Err(PathError::ContainsNul));

    let too_many = vec!["x"; 257].join("/");
    assert_eq!(Path::new(&too_many), Err(PathError::TooManyComponents));
    let too_long = "x".repeat(MAX_PATH_BYTES + 1);
    assert_eq!(Path::new(&too_long), Err(PathError::TooLong));
}

#[test]
fn synthesizes_a_hierarchy_and_exposes_stable_relations() {
    let bytes = archive(&[
        ("var/log/messages", 0o100_640, b"abcdef"),
        ("etc/config", 0o100_644, b"value"),
        ("etc", 0o040_750, b""),
        ("link", 0o120_777, b"etc/config"),
    ]);
    let filesystem = crate::require_ok(RamFs::from_newc(&bytes));
    assert_eq!(filesystem.nodes().len(), 7);

    let root = filesystem.root();
    let etc = crate::require_some(crate::require_ok(
        filesystem.lookup_child(root.id(), name("etc")),
    ));
    assert_eq!(etc.parent(), root.id());
    assert_eq!(etc.kind(), NodeKind::Directory);
    assert_eq!(etc.mode(), 0o040_750);
    let config = crate::require_some(crate::require_ok(
        filesystem.lookup_child(etc.id(), name("config")),
    ));
    assert_eq!(config.path(), "etc/config");
    assert_eq!(config.name(), "config");
    assert_eq!(config.parent(), etc.id());
    assert_eq!(
        crate::require_ok(filesystem.attributes(config.id())).size(),
        5
    );

    let mut read = [0u8; 3];
    assert_eq!(
        crate::require_ok(filesystem.read_at(config.id(), 1, &mut read)),
        3
    );
    assert_eq!(&read, b"alu");
    assert_eq!(
        crate::require_ok(filesystem.read_at(config.id(), 100, &mut read)),
        0
    );

    let var = crate::require_some(crate::require_ok(
        filesystem.lookup_child(root.id(), name("var")),
    ));
    let log = crate::require_some(crate::require_ok(
        filesystem.lookup_child(var.id(), name("log")),
    ));
    assert_eq!(var.kind(), NodeKind::Directory);
    assert_eq!(log.kind(), NodeKind::Directory);
    assert_eq!(var.mode(), 0o040_755);
    assert_eq!(log.mode(), 0o040_755);

    let link = crate::require_some(crate::require_ok(
        filesystem.lookup_child(root.id(), name("link")),
    ));
    let mut target = [0u8; 16];
    let length = crate::require_ok(filesystem.read_link(link.id(), &mut target));
    assert_eq!(target.get(..length), Some(b"etc/config".as_slice()));
}

#[test]
fn enumerates_direct_children_in_name_order_with_resumable_cookies() {
    let bytes = archive(&[
        ("z/file", 0o100_644, b"z"),
        ("a", 0o100_644, b"a"),
        ("middle", 0o040_755, b""),
    ]);
    let filesystem = crate::require_ok(RamFs::from_newc(&bytes));
    let mut entries =
        crate::require_ok(filesystem.enumerate(filesystem.root().id(), DirectoryCookie::START));
    assert_eq!(entries.len(), 3);
    let first = crate::require_some(entries.next());
    let second = crate::require_some(entries.next());
    assert_eq!(first.node().name(), "a");
    assert_eq!(second.node().name(), "middle");

    let resumed: Vec<_> =
        crate::require_ok(filesystem.enumerate(filesystem.root().id(), second.next_cookie()))
            .map(|entry| entry.node().name())
            .collect();
    assert_eq!(resumed, ["z"]);
    assert!(matches!(
        filesystem.enumerate(filesystem.root().id(), DirectoryCookie::new(4)),
        Err(RamFsError::InvalidDirectoryCookie)
    ));
}

#[test]
fn assigns_node_ids_deterministically_after_canonical_sorting() {
    let first = archive(&[("z/file", 0o100_644, b"z"), ("a/file", 0o100_644, b"a")]);
    let second = archive(&[("a/file", 0o100_644, b"a"), ("z/file", 0o100_644, b"z")]);
    let first = crate::require_ok(RamFs::from_newc(&first));
    let second = crate::require_ok(RamFs::from_newc(&second));
    let first_ids: Vec<_> = first.nodes().map(|node| (node.path(), node.id())).collect();
    let second_ids: Vec<_> = second
        .nodes()
        .map(|node| (node.path(), node.id()))
        .collect();
    assert_eq!(first_ids, second_ids);
}

#[test]
fn rejects_every_archive_order_for_non_directory_ancestors() {
    for entries in [
        [
            ("tree", 0o100_644, b"file".as_slice()),
            ("tree/leaf", 0o100_644, b"leaf".as_slice()),
        ],
        [
            ("tree/leaf", 0o100_644, b"leaf".as_slice()),
            ("tree", 0o100_644, b"file".as_slice()),
        ],
        [
            ("tree", 0o120_777, b"target".as_slice()),
            ("tree/leaf", 0o100_644, b"leaf".as_slice()),
        ],
    ] {
        let bytes = archive(&entries);
        assert_eq!(
            RamFs::from_newc(&bytes).map(|_| ()),
            Err(RamFsError::AncestorNotDirectory)
        );
    }
}

#[test]
fn type_specific_operations_reject_the_wrong_node_kind() {
    let bytes = archive(&[("file", 0o100_644, b"data"), ("dir", 0o040_755, b"")]);
    let filesystem = crate::require_ok(RamFs::from_newc(&bytes));
    let root = filesystem.root();
    let file = crate::require_some(crate::require_ok(
        filesystem.lookup_child(root.id(), name("file")),
    ));
    let directory = crate::require_some(crate::require_ok(
        filesystem.lookup_child(root.id(), name("dir")),
    ));
    assert!(matches!(
        filesystem.enumerate(file.id(), DirectoryCookie::START),
        Err(RamFsError::NotDirectory)
    ));
    assert_eq!(
        filesystem.read_at(directory.id(), 0, &mut [0u8; 1]),
        Err(RamFsError::NotRegularFile)
    );
    assert_eq!(
        filesystem.read_link(file.id(), &mut [0u8; 4]),
        Err(RamFsError::NotSymlink)
    );
}

#[test]
fn bounds_archive_expansion_work_before_normalization() {
    let deep_path = vec!["d"; 256].join("/");
    let mut bytes = Vec::new();
    for _ in 0..=1024 {
        append_entry(&mut bytes, &deep_path, 0o100_644, b"x");
    }
    append_entry(&mut bytes, "TRAILER!!!", 0, &[]);
    assert_eq!(
        RamFs::from_newc(&bytes).map(|_| ()),
        Err(RamFsError::BuildWorkBudgetExceeded)
    );
}
