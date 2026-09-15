#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0
"""Compose checksum-pinned Alpine userspace with matching boot modules."""
import argparse
import hashlib
import gzip
import stat
import os
import posixpath
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

PAYLOADS = {
    'aarch64': ('3.23.5', 'd9a77cb31f715c56afa4f0a5aa42c04cfde813b70ad74a64725902b09c29a6cc'),
    'riscv64': ('3.24.1', '7201513262d851f39105102cf95519410100259bd7996fca13bade517838d7b7'),
    'x86_64': ('3.23.5', 'fae0d78ad39563573ddececfdd55ae1040ed428442e95ea5401cf66d9079b327'),
}
MODLOOP_SHA = '83599ae8dbdd48ab452600c00bce8dd1c0c77075223196732cd99b7f8d774f89'


def fetch(url, checksum, cache):
    cache.mkdir(parents=True, exist_ok=True)
    destination = cache / checksum
    def valid(path):
        return path.is_file() and hashlib.sha256(path.read_bytes()).hexdigest() == checksum
    if not valid(destination):
        with tempfile.TemporaryDirectory(dir=cache) as temporary:
            download = Path(temporary) / 'download'
            subprocess.run(['curl', '-fL', url, '-o', str(download)], check=True)
            if not valid(download):
                raise ValueError(f'checksum mismatch: {url}')
            os.replace(download, destination)
    return destination


def initramfs(archive, output):
    # Encode from archive metadata: an unprivileged host extraction cannot
    # retain Linux uid/gid, and cpio -R would discard distribution groups.
    with tarfile.open(archive) as source, output.open('wb') as raw:
        with gzip.GzipFile(fileobj=raw, mode='wb', mtime=0, filename='') as target:
            def entry(ino, name, mode, uid, gid, mtime, data):
                name = name.encode() + b'\0'
                fields = (ino, mode, uid, gid, 1, mtime, len(data), 0, 0, 0, 0, len(name), 0)
                header = b'070701' + ''.join(f'{value:08x}' for value in fields).encode()
                target.write(header + name)
                target.write(b'\0' * (-(len(header) + len(name)) % 4))
                target.write(data)
                target.write(b'\0' * (-len(data) % 4))
            for ino, member in enumerate(source, 1):
                if member.isdir():
                    mode, data = stat.S_IFDIR, b''
                elif member.issym():
                    mode, data = stat.S_IFLNK, member.linkname.encode()
                elif member.isfile() or member.islnk():
                    mode, data = stat.S_IFREG, source.extractfile(member).read()
                else:
                    raise ValueError(f'unsupported rootfs entry: {member.name}')
                entry(ino, member.name, mode | member.mode, member.uid, member.gid,
                      int(member.mtime), data)
            entry(0, 'TRAILER!!!', 0, 0, 0, 0, b'')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--arch', choices=PAYLOADS, required=True)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    version, checksum = PAYLOADS[args.arch]
    base = f'https://dl-cdn.alpinelinux.org/alpine/v{version.rsplit(".", 1)[0]}/releases/{args.arch}'
    cache = Path(tempfile.gettempdir()) / 'hyper-alpine-rootfs-cache'
    archive = fetch(f'{base}/alpine-minirootfs-{version}-{args.arch}.tar.gz', checksum, cache)
    # Start from distribution userspace, not the rescue BusyBox binary or its
    # merged-/usr symlinks. Preserve only the netboot kernel modules.
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary) / 'root'
        root.mkdir()
        with tarfile.open(archive) as source:
            metadata = {m.name.removeprefix("./").rstrip("/"): m for m in source}
            def guest_links(member, destination):
                # Alpine applets point at /bin/busybox. Make those links relative
                # inside the staging tree instead of pointing at the host /bin.
                if member.issym() and member.linkname.startswith('/'):
                    member = member.replace(linkname=posixpath.relpath(
                        member.linkname, '/' + posixpath.dirname(member.name)))
                return tarfile.data_filter(member, destination)
            source.extractall(root, filter=guest_links)
        modules = args.root / 'lib/modules'
        if modules.exists():
            shutil.copytree(modules, root / 'lib/modules', symlinks=True)
        if args.arch == 'aarch64':
            modloop = fetch(f'{base}/netboot-{version}/modloop-virt', MODLOOP_SHA, cache)
            tool = shutil.which('unsquashfs')
            if not tool:
                raise ValueError('unsquashfs is required (install squashfs-tools or Homebrew squashfs)')
            extracted = Path(temporary) / 'modloop'
            subprocess.run([tool, '-no-progress', '-d', str(extracted), str(modloop)], check=True)
            # Include the block/filesystem module dependency closure, not every
            # driver in the distribution's modloop.
            versions = [p for p in (extracted / 'modules').iterdir() if (p / 'modules.dep').is_file()]
            if len(versions) != 1 or versions[0].name not in [p.name for p in modules.iterdir()]:
                raise ValueError('modloop and netboot kernel versions differ')
            source = versions[0]
            dependencies = {}
            for line in (source / 'modules.dep').read_text().splitlines():
                module, needed = line.split(':', 1)
                dependencies[module] = needed.split()
            selected = set()
            def include(module):
                if module in selected:
                    return
                selected.add(module)
                for dep in dependencies[module]:
                    include(dep)
            for name in ('virtio_mmio', 'virtio_scsi', 'sd_mod', 'ext4'):
                matches = [p for p in dependencies if Path(p).stem == name]
                if len(matches) != 1:
                    raise ValueError(f'missing module {name}')
                include(matches[0])
            destination = root / 'lib/modules' / source.name
            for module in selected:
                target = destination / module
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source / module, target)
            # BusyBox modprobe consumes the text dependency table.
            shutil.copyfile(source / 'modules.dep', destination / 'modules.dep')
        shutil.copyfile(Path(__file__).with_name('init'), root / 'init')
        (root / 'init').chmod(0o755)
        # A tar preserves Linux mode/ownership independently of host uid/gid.
        archive_output = args.output / 'rootfs.tar'
        with tarfile.open(archive_output.with_suffix('.tar.tmp'), 'w') as target:
            def ownership(info):
                original = metadata.get(info.name)
                info.uid = original.uid if original else 0
                info.gid = original.gid if original else 0
                if original:
                    info.mode = original.mode
                info.uname = info.gname = ''
                return info
            for item in sorted(root.iterdir()):
                target.add(item, arcname=item.name, filter=ownership)
        os.replace(archive_output.with_suffix('.tar.tmp'), archive_output)
        ramdisk = args.output / 'initramfs.cpio.gz'
        initramfs(archive_output, ramdisk.with_suffix('.gz.tmp'))
        os.replace(ramdisk.with_suffix('.gz.tmp'), ramdisk)


if __name__ == '__main__':
    main()
