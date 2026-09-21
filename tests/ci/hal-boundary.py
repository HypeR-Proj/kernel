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
    required = ('hyper', 'hyper-hal', 'hyper-core')
    if any(name not in packages for name in required):
        raise ValueError('kernel, HAL and reusable core must be separate Cargo packages')
    kernel, hal, core = (packages[name]['id'] for name in required)
    edges = {
        node['id']: {dependency['pkg'] for dependency in node['deps']}
        for node in metadata['resolve']['nodes']
    }
    if not {hal, core} <= edges[kernel] or core not in edges[hal]:
        raise ValueError('required dependency direction is kernel -> HAL/core and HAL -> core')
    for origin, forbidden in ((hal, kernel), (core, kernel), (core, hal)):
        pending = list(edges[origin])
        visited = set()
        while pending:
            dependency = pending.pop()
            if dependency == forbidden:
                raise ValueError(f'forbidden reverse Cargo dependency: {origin} -> {forbidden}')
            if dependency not in visited:
                visited.add(dependency)
                pending.extend(edges.get(dependency, ()))
    if any('lib' in target['kind'] for target in packages['hyper']['targets']):
        raise ValueError('kernel policy must not be published as the reusable core library')


def private_arch_error(output):
    for line in output.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get('reason') != 'compiler-message':
            continue
        diagnostic = message.get('message', {})
        if ((diagnostic.get('code') or {}).get('code') == 'E0603'
                and diagnostic.get('message') == 'module `arch` is private'):
            return True
    return False


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
    target = os.environ.get('HYPER_HAL_BOUNDARY_TARGET', 'aarch64-unknown-none')
    configs = {
        'aarch64-unknown-none': 'qemu_aarch64_defconfig',
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
        if negative.returncode == 0 or not private_arch_error(negative.stdout):
            raise RuntimeError('private architecture import did not fail with E0603:\n'
                               + negative.stdout + negative.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('check', choices=('graph', 'privacy'))
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[2])
    arguments = parser.parse_args()
    root = arguments.root.resolve()
    if arguments.check == 'graph':
        check_graph(root)
    else:
        check_privacy(root)
    print(f'HAL {arguments.check} boundary verified')


if __name__ == '__main__':
    main()
