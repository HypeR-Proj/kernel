#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Check Cargo dependency direction and actual HAL privacy with rustc."""

import argparse
import json
import os
import shutil
from pathlib import Path
import subprocess
import tempfile


def verify_graph(metadata):
    packages = {package['name']: package for package in metadata['packages']}
    required = {
        'hyper': {'hyper-core', 'hyper-hal', 'hyper-platform', 'hyper-drivers',
                  'hyper-vm-models', 'hyper-trace'},
        'hyper-hal': {'hyper-core', 'hyper-platform', 'hyper-drivers', 'hyper-vm-models'},
        'hyper-drivers': {'hyper-core', 'hyper-platform'},
        'hyper-platform': {'hyper-core'},
        'hyper-vm-models': {'hyper-core'},
        'hyper-trace': {'hyper-core'},
        'hyper-core': set(),
    }
    if missing := required.keys() - packages.keys():
        raise ValueError(f'kernel subsystems must be separate Cargo packages: {sorted(missing)}')
    edges = {
        node['id']: {dependency['pkg'] for dependency in node['deps']}
        for node in metadata['resolve']['nodes']
    }
    ids = {name: packages[name]['id'] for name in required}
    for name, dependencies in required.items():
        if not {ids[dependency] for dependency in dependencies} <= edges[ids[name]]:
            raise ValueError(f'required dependency direction violated for {name}')
        # The declared graph includes each layer's transitive lower layers.
        # Traverse external helper crates too: they cannot hide a reverse edge.
        forbidden = {ids[other] for other in required if other not in dependencies}
        pending = list(edges[ids[name]])
        visited = set()
        while pending:
            dependency = pending.pop()
            if dependency in forbidden:
                raise ValueError(f'forbidden reverse Cargo dependency: {name} -> {dependency}')
            if dependency not in visited:
                visited.add(dependency)
                pending.extend(edges.get(dependency, ()))
    if any('lib' in target['kind'] for target in packages['hyper']['targets']):
        raise ValueError('kernel policy must not be published as the reusable core library')


def private_arch_error(output, source=Path('src/lib.rs')):
    """Only the expected rustc error at the probe import proves privacy."""
    errors = []
    for line in output.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get('reason') != 'compiler-message':
            continue
        diagnostic = message.get('message', {})
        if diagnostic.get('level') == 'error':
            errors.append((message.get('target', {}).get('src_path', ''), diagnostic))
    return bool(errors) and all(
        Path(target).resolve() == source.resolve()
        and (diagnostic.get('code') or {}).get('code') == 'E0603'
        and any(span.get('is_primary') and span.get('line_start') == 2
                and span.get('file_name') in ('src/lib.rs', str(source))
                for span in diagnostic.get('spans', []))
        for target, diagnostic in errors
    )


def cargo_command():
    return os.environ.get('CARGO', 'cargo')


def check_graph(root):
    metadata = subprocess.run(
        [cargo_command(), 'metadata', '--locked', '--format-version=1',
         '--manifest-path', str(root / 'Cargo.toml')],
        cwd=root, check=True, capture_output=True, text=True,
    )
    verify_graph(json.loads(metadata.stdout))


def check_privacy(root):
    target = os.environ.get('HYPER_HAL_BOUNDARY_TARGET', 'aarch64-unknown-none-softfloat')
    configs = {
        'aarch64-unknown-none-softfloat': 'qemu_aarch64_defconfig',
        'riscv64imac-unknown-none-elf': 'qemu_riscv64_defconfig',
        'x86_64-unknown-none': 'qemu_x86_64_defconfig',
    }
    environment = os.environ.copy()
    environment['HYPER_CONFIG'] = str(root / 'configs' / configs[target])
    with tempfile.TemporaryDirectory(prefix='hyper-hal-boundary-') as temporary:
        fixture = Path(temporary)
        (fixture / 'src').mkdir()
        (fixture / 'Cargo.toml').write_text(
            '[package]\nname = "hyper-hal-boundary-probe"\nversion = "0.0.0"\n'
            'edition = "2024"\n[workspace]\n[dependencies]\n'
            f'hyper-hal = {{ path = {json.dumps(str(root / "hal"))} }}\n'
        )
        # Reuse the project's dependency versions; only the temporary probe
        # package is added to this private lockfile.
        shutil.copyfile(root / 'Cargo.lock', fixture / 'Cargo.lock')
        source = fixture / 'src/lib.rs'
        command = [cargo_command(), 'check', '--lib', '--target', target,
                   '--manifest-path', str(fixture / 'Cargo.toml'),
                   '--target-dir', str(root / 'target/hal-boundary'),
                   '--message-format=json']
        source.write_text('#![no_std]\npub use hyper_hal::cpu;\n')
        positive = subprocess.run(command, cwd=root, env=environment,
                                  capture_output=True, text=True)
        if positive.returncode:
            raise RuntimeError('public HAL interface failed to compile:\n'
                               + positive.stdout + positive.stderr)
        source.write_text('#![no_std]\npub use hyper_hal::arch;\n')
        negative = subprocess.run(command, cwd=root, env=environment,
                                  capture_output=True, text=True)
        if negative.returncode == 0 or not private_arch_error(negative.stdout, source):
            raise RuntimeError('private architecture import did not fail with E0603:\n'
                               + negative.stdout + negative.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('check', choices=('graph', 'privacy', 'all'), nargs='?', default='all')
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[2])
    arguments = parser.parse_args()
    root = arguments.root.resolve()
    if arguments.check in ('graph', 'all'):
        check_graph(root)
    if arguments.check in ('privacy', 'all'):
        check_privacy(root)
    print(f'HAL {arguments.check} boundary verified')


if __name__ == '__main__':
    main()
