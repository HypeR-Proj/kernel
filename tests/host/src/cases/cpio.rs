// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! CPIO parsing and nested VM package selection contracts.

use hyper::archive::cpio::{Archive, EntryKind, Error};
use hyper::fs::ramfs::{Error as RamFsError, NodeKind, RamFs};
use hyper::vm::bundle;

fn append_hex(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(format!("{value:08x}").as_bytes());
}

fn append_entry(output: &mut Vec<u8>, name: &str, mode: u32, data: &[u8]) {
    append_entry_with_checksum(output, name, mode, data, false);
}

fn append_entry_with_checksum(
    output: &mut Vec<u8>,
    name: &str,
    mode: u32,
    data: &[u8],
    checksum: bool,
) {
    output.extend_from_slice(if checksum { b"070702" } else { b"070701" });
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
    append_hex(
        output,
        if checksum {
            data.iter()
                .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)))
        } else {
            0
        },
    );
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

fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut output = Vec::new();
    for (name, data) in entries {
        append_entry(&mut output, name, 0o100_644, data);
    }
    append_entry(&mut output, "TRAILER!!!", 0, &[]);
    output
}

fn archive_with_modes(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
    let mut output = Vec::new();
    for (name, mode, data) in entries {
        append_entry(&mut output, name, *mode, data);
    }
    append_entry(&mut output, "TRAILER!!!", 0, &[]);
    output
}

fn boot_archive(boot: &[u8], bundle: &[u8]) -> Vec<u8> {
    archive(&[
        ("hypervisor/boot.conf", boot),
        ("hypervisor/vms/demo.cpio", bundle),
    ])
}

fn bundle_archive(manifest: &[u8]) -> Vec<u8> {
    archive(&[("manifest", manifest), ("kernel/Image", b"kernel")])
}

fn manifest_with(memory: &str, vcpus: &str, command_line: &str) -> String {
    format!(
        "format=hyper-vm-v1\ntype=linux\narchitecture=aarch64\nmemory={memory}\nvcpus={vcpus}\ncommand_line={command_line}\nkernel=kernel/Image\ninitramfs=\n"
    )
}

#[test]
fn parses_newc_files_without_allocating_in_the_parser() {
    let bytes = archive(&[("hypervisor/boot.conf", b"default=demo"), ("empty", b"")]);
    let archive = crate::require_ok(Archive::new(&bytes));
    let entry = crate::require_some(crate::require_ok(
        archive.find_unique("hypervisor/boot.conf"),
    ));
    assert_eq!(entry.kind(), EntryKind::File);
    assert_eq!(entry.data(), b"default=demo");
    assert_eq!(crate::require_ok(archive.find_unique("missing")), None);
}

#[test]
fn rejects_duplicate_and_truncated_entries() {
    let duplicate = archive(&[("manifest", b"one"), ("manifest", b"two")]);
    let parsed = crate::require_ok(Archive::new(&duplicate));
    assert_eq!(parsed.find_unique("manifest"), Err(Error::DuplicateEntry));

    let mut truncated = archive(&[("manifest", b"payload")]);
    truncated.truncate(truncated.len() - 120);
    assert!(Archive::new(&truncated).is_err());
}

#[test]
fn validates_crc_archives() {
    let mut bytes = Vec::new();
    append_entry_with_checksum(&mut bytes, "payload", 0o100_644, b"checked", true);
    append_entry(&mut bytes, "TRAILER!!!", 0, &[]);
    let parsed = crate::require_ok(Archive::new(&bytes));
    let payload = crate::require_some(crate::require_ok(parsed.find_unique("payload")));
    assert_eq!(payload.data(), b"checked");

    let data_offset = payload.data().as_ptr() as usize - bytes.as_ptr() as usize;
    bytes[data_offset] ^= 1;
    assert_eq!(
        Archive::new(&bytes).map(|_| ()),
        Err(Error::InvalidChecksum)
    );

    let mut newc_with_checksum = archive(&[("payload", b"unchecked")]);
    newc_with_checksum[102..110].copy_from_slice(b"00000001");
    assert_eq!(
        Archive::new(&newc_with_checksum).map(|_| ()),
        Err(Error::InvalidChecksum)
    );
}

#[test]
fn mounts_an_immutable_root_filesystem_with_canonical_lookup() {
    let bytes = archive_with_modes(&[
        (".", 0o040_755, b""),
        ("etc", 0o040_755, b""),
        ("etc/config", 0o100_644, b"value"),
        ("./init", 0o100_755, b"native image"),
    ]);
    let root = crate::require_ok(RamFs::from_newc(&bytes));

    assert_eq!(root.nodes().len(), 3);
    let init = crate::require_some(crate::require_ok(root.lookup("/init")));
    assert_eq!(init.kind(), NodeKind::File);
    assert!(init.is_executable());
    assert_eq!(init.data(), b"native image");
    let config = crate::require_some(crate::require_ok(root.lookup("/etc/config")));
    assert_eq!(config.data(), b"value");
    assert_eq!(crate::require_ok(root.lookup("/missing")), None);
}

#[test]
fn rejects_ambiguous_or_escaping_ramfs_paths() {
    let duplicate = archive(&[("init", b"one"), ("./init", b"two")]);
    assert_eq!(
        RamFs::from_newc(&duplicate).map(|_| ()),
        Err(RamFsError::DuplicatePath)
    );

    for path in ["/absolute", "../escape", "dir/../escape", "dir//file"] {
        let bytes = archive(&[(path, b"payload")]);
        assert_eq!(
            RamFs::from_newc(&bytes).map(|_| ()),
            Err(RamFsError::InvalidPath)
        );
    }

    let root_file = archive_with_modes(&[(".", 0o100_755, b"payload")]);
    assert_eq!(
        RamFs::from_newc(&root_file).map(|_| ()),
        Err(RamFsError::InvalidPath)
    );
}

#[test]
fn loads_a_versioned_nested_vm_bundle() {
    let mut kernel = [0u8; 64];
    kernel[56..60].copy_from_slice(b"ARMd");
    let manifest = b"format=hyper-vm-v1\ntype=linux\narchitecture=aarch64\nmemory=134217728\nvcpus=1\ncommand_line=console=ttyAMA0 rdinit=/init\nkernel=kernel/Image\ninitramfs=initramfs/root.cpio\n";
    let inner = archive(&[
        ("manifest", manifest),
        ("kernel/Image", &kernel),
        ("initramfs/root.cpio", b"guest initramfs"),
    ]);
    let outer = archive(&[
        (
            "hypervisor/boot.conf",
            b"format=hyper-boot-v1\ndefault=demo\n",
        ),
        ("hypervisor/vms/demo.cpio", &inner),
    ]);
    let guest = crate::require_ok(bundle::select_default(&outer));
    assert_eq!(guest.name(), "demo");
    assert_eq!(guest.guest_type(), "linux");
    assert_eq!(guest.architecture(), "aarch64");
    assert_eq!(guest.memory_size(), 128 * 1024 * 1024);
    assert_eq!(guest.vcpu_count(), 1);
    assert_eq!(guest.kernel(), &kernel);
    assert_eq!(guest.initramfs(), Some(b"guest initramfs".as_slice()));
}

#[test]
fn rejects_unknown_manifest_properties() {
    let mut kernel = [0u8; 64];
    kernel[56..60].copy_from_slice(b"ARMd");
    let manifest = b"format=hyper-vm-v1\ntype=linux\narchitecture=aarch64\nmemory=134217728\nvcpus=1\ncommand_line=console=ttyAMA0\nkernel=kernel/Image\ninitramfs=\ntypo=value\n";
    let inner = archive(&[("manifest", manifest), ("kernel/Image", &kernel)]);
    let outer = archive(&[
        (
            "hypervisor/boot.conf",
            b"format=hyper-boot-v1\ndefault=demo\n",
        ),
        ("hypervisor/vms/demo.cpio", &inner),
    ]);
    assert_eq!(
        bundle::select_default(&outer).map(|_| ()),
        Err(bundle::Error::UnknownProperty)
    );
}

#[test]
fn keeps_package_parsing_independent_from_runner_capabilities() {
    let manifest = b"format=hyper-vm-v1\ntype=freebsd\narchitecture=x86_64\nmemory=134217728\nvcpus=64\ncommand_line=\nkernel=kernel/guest.bin\ninitramfs=\n";
    let inner = archive(&[("manifest", manifest), ("kernel/guest.bin", b"opaque")]);
    let outer = archive(&[
        (
            "hypervisor/boot.conf",
            b"format=hyper-boot-v1\ndefault=portable\n",
        ),
        ("hypervisor/vms/portable.cpio", &inner),
    ]);
    let guest = crate::require_ok(bundle::select_default(&outer));
    assert_eq!(guest.guest_type(), "freebsd");
    assert_eq!(guest.architecture(), "x86_64");
    assert_eq!(guest.vcpu_count(), 64);
    assert_eq!(guest.command_line(), "");
    assert_eq!(guest.kernel(), b"opaque");
}

#[test]
fn reports_the_archive_layer_that_is_malformed() {
    assert!(matches!(
        bundle::select_default(b"invalid"),
        Err(bundle::Error::BootArchive(_))
    ));

    let outer = archive(&[
        (
            "hypervisor/boot.conf",
            b"format=hyper-boot-v1\ndefault=broken\n",
        ),
        ("hypervisor/vms/broken.cpio", b"invalid"),
    ]);
    assert!(matches!(
        bundle::select_default(&outer),
        Err(bundle::Error::BundleArchive(_))
    ));
}

#[test]
fn rejects_missing_and_duplicate_configuration_properties() {
    let cases: &[(&[u8], bundle::Error)] = &[
        (b"format=hyper-boot-v1\n", bundle::Error::MissingProperty),
        (
            b"format=hyper-boot-v1\ndefault=demo\ndefault=other\n",
            bundle::Error::DuplicateProperty,
        ),
    ];
    for (boot, expected) in cases {
        let outer = boot_archive(
            boot,
            &bundle_archive(manifest_with("67108864", "1", "").as_bytes()),
        );
        assert_eq!(bundle::select_default(&outer).map(|_| ()), Err(*expected));
    }

    let manifests: &[(&[u8], bundle::Error)] = &[
        (
            b"format=hyper-vm-v1\ntype=linux\narchitecture=aarch64\nmemory=67108864\nvcpus=1\ncommand_line=\ninitramfs=\n",
            bundle::Error::MissingProperty,
        ),
        (
            b"format=hyper-vm-v1\ntype=linux\ntype=freebsd\narchitecture=aarch64\nmemory=67108864\nvcpus=1\ncommand_line=\nkernel=kernel/Image\ninitramfs=\n",
            bundle::Error::DuplicateProperty,
        ),
    ];
    for (manifest, expected) in manifests {
        let inner = bundle_archive(manifest);
        let outer = boot_archive(b"format=hyper-boot-v1\ndefault=demo\n", &inner);
        assert_eq!(bundle::select_default(&outer).map(|_| ()), Err(*expected));
    }
}

#[test]
fn rejects_unsafe_vm_names_and_payload_paths() {
    for name in ["", ".", "..", "../demo", "demo/child"] {
        let boot = format!("format=hyper-boot-v1\ndefault={name}\n");
        let outer = archive(&[("hypervisor/boot.conf", boot.as_bytes())]);
        assert_eq!(
            bundle::select_default(&outer).map(|_| ()),
            Err(bundle::Error::InvalidVmName)
        );
    }
    let long_name = "v".repeat(65);
    let boot = format!("format=hyper-boot-v1\ndefault={long_name}\n");
    let outer = archive(&[("hypervisor/boot.conf", boot.as_bytes())]);
    assert_eq!(
        bundle::select_default(&outer).map(|_| ()),
        Err(bundle::Error::InvalidVmName)
    );

    for path in [
        "/kernel/Image",
        "kernel//Image",
        "../Image",
        "kernel/../Image",
        "kernel/",
    ] {
        let manifest = format!(
            "format=hyper-vm-v1\ntype=linux\narchitecture=aarch64\nmemory=67108864\nvcpus=1\ncommand_line=\nkernel={path}\ninitramfs=\n"
        );
        let inner = bundle_archive(manifest.as_bytes());
        let outer = boot_archive(b"format=hyper-boot-v1\ndefault=demo\n", &inner);
        assert_eq!(
            bundle::select_default(&outer).map(|_| ()),
            Err(bundle::Error::InvalidPath)
        );
    }
}

#[test]
fn validates_memory_and_vcpu_numeric_boundaries() {
    for (memory, expected) in [
        ("67108863", bundle::Error::InvalidMemorySize),
        ("100663296", bundle::Error::InvalidMemorySize),
        ("18446744073709551616", bundle::Error::InvalidMemorySize),
    ] {
        let inner = bundle_archive(manifest_with(memory, "1", "").as_bytes());
        let outer = boot_archive(b"format=hyper-boot-v1\ndefault=demo\n", &inner);
        assert_eq!(bundle::select_default(&outer).map(|_| ()), Err(expected));
    }
    for vcpus in ["0", "4294967296"] {
        let inner = bundle_archive(manifest_with("67108864", vcpus, "").as_bytes());
        let outer = boot_archive(b"format=hyper-boot-v1\ndefault=demo\n", &inner);
        assert_eq!(
            bundle::select_default(&outer).map(|_| ()),
            Err(bundle::Error::InvalidVcpuCount)
        );
    }

    for (memory, vcpus, expected_vcpus) in
        [("67108864", "1", 1), ("67108864", "4294967295", u32::MAX)]
    {
        let inner = bundle_archive(manifest_with(memory, vcpus, "").as_bytes());
        let outer = boot_archive(b"format=hyper-boot-v1\ndefault=demo\n", &inner);
        let guest = crate::require_ok(bundle::select_default(&outer));
        assert_eq!(guest.memory_size(), 64 * 1024 * 1024);
        assert_eq!(guest.vcpu_count(), expected_vcpus);
    }
}

#[test]
fn validates_command_line_boundaries_and_text() {
    let maximum = "x".repeat(2048);
    let inner = bundle_archive(manifest_with("67108864", "1", &maximum).as_bytes());
    let outer = boot_archive(b"format=hyper-boot-v1\ndefault=demo\n", &inner);
    assert_eq!(
        crate::require_ok(bundle::select_default(&outer)).command_line(),
        maximum
    );

    let too_long = "x".repeat(2049);
    let inner = bundle_archive(manifest_with("67108864", "1", &too_long).as_bytes());
    let outer = boot_archive(b"format=hyper-boot-v1\ndefault=demo\n", &inner);
    assert_eq!(
        bundle::select_default(&outer).map(|_| ()),
        Err(bundle::Error::InvalidCommandLine)
    );

    let manifest = manifest_with("67108864", "1", "contains\0nul");
    let inner = bundle_archive(manifest.as_bytes());
    let outer = boot_archive(b"format=hyper-boot-v1\ndefault=demo\n", &inner);
    assert_eq!(
        bundle::select_default(&outer).map(|_| ()),
        Err(bundle::Error::InvalidText)
    );
}

#[test]
fn rejects_non_regular_required_entries() {
    const DIRECTORY: u32 = 0o040_755;
    const FILE: u32 = 0o100_644;
    let valid_boot = b"format=hyper-boot-v1\ndefault=demo\n";
    let valid_manifest = manifest_with("67108864", "1", "");
    let valid_inner = bundle_archive(valid_manifest.as_bytes());

    let outer = archive_with_modes(&[("hypervisor/boot.conf", DIRECTORY, valid_boot)]);
    assert_eq!(
        bundle::select_default(&outer).map(|_| ()),
        Err(bundle::Error::NotRegularFile)
    );

    let inner = archive_with_modes(&[
        ("manifest", DIRECTORY, valid_manifest.as_bytes()),
        ("kernel/Image", FILE, b"kernel"),
    ]);
    let outer = boot_archive(valid_boot, &inner);
    assert_eq!(
        bundle::select_default(&outer).map(|_| ()),
        Err(bundle::Error::NotRegularFile)
    );

    let inner = archive_with_modes(&[
        ("manifest", FILE, valid_manifest.as_bytes()),
        ("kernel/Image", DIRECTORY, b"kernel"),
    ]);
    let outer = boot_archive(valid_boot, &inner);
    assert_eq!(
        bundle::select_default(&outer).map(|_| ()),
        Err(bundle::Error::NotRegularFile)
    );

    let outer = archive_with_modes(&[
        ("hypervisor/boot.conf", FILE, valid_boot),
        ("hypervisor/vms/demo.cpio", DIRECTORY, &valid_inner),
    ]);
    assert_eq!(
        bundle::select_default(&outer).map(|_| ()),
        Err(bundle::Error::NotRegularFile)
    );

    let manifest = b"format=hyper-vm-v1\ntype=linux\narchitecture=aarch64\nmemory=67108864\nvcpus=1\ncommand_line=\nkernel=kernel/Image\ninitramfs=initramfs/root.cpio\n";
    let inner = archive_with_modes(&[
        ("manifest", FILE, manifest),
        ("kernel/Image", FILE, b"kernel"),
        ("initramfs/root.cpio", DIRECTORY, b"initramfs"),
    ]);
    let outer = boot_archive(valid_boot, &inner);
    assert_eq!(
        bundle::select_default(&outer).map(|_| ()),
        Err(bundle::Error::NotRegularFile)
    );
}
