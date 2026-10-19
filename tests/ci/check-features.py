#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Compile production capabilities and reject independently enabled test hooks."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tomllib

KERNEL = Path(__file__).resolve().parents[2]
TARGET = 'aarch64-unknown-none-softfloat'
TEST_FEATURES = {
    'hyper': ('kernel-self-test', 'kernel-smmuv3-test', 'kernel-vgic-tc-test',
              'io-dma-revoke-test', 'kernel-stack-audit'),
    'hyper-core': ('kernel-self-test', 'io-dma-revoke-test', 'kernel-stack-audit'),
    'hyper-hal': ('kernel-self-test', 'kernel-vgic-tc-test', 'kernel-stack-audit'),
}


def feature_contract():
    """Require new manifest features to be classified before matrix admission."""
    paths = {'hyper': KERNEL / 'Cargo.toml', 'hyper-core': KERNEL / 'core/Cargo.toml',
             'hyper-hal': KERNEL / 'hal/Cargo.toml'}
    manifests = {package: tomllib.loads(path.read_text()) for package, path in paths.items()}
    production = manifests['hyper']['package']['metadata']['hyper']['production-features']
    if (not isinstance(production, list) or not production
            or any(not isinstance(name, str) for name in production)
            or len(production) != len(set(production))):
        raise RuntimeError('kernel production feature metadata must contain unique names')
    for package, manifest in manifests.items():
        declared = set(manifest['features'])
        tests = set(TEST_FEATURES[package])
        if missing := tests - declared:
            raise RuntimeError(f'{package}: matrix names undeclared test features: {sorted(missing)}')
        known = {'default', 'kernel-test', *production, *tests}
        if unknown := declared - known:
            raise RuntimeError(f'{package}: unclassified features require review: {sorted(unknown)}')
        if tests.intersection(production) or 'kernel-test' in production:
            raise RuntimeError('kernel test features cannot be classified as production')
    if missing := set(production) - manifests['hyper']['features'].keys():
        raise RuntimeError(f'undeclared production features: {sorted(missing)}')
    return production


def run(command, environment, log):
    result = subprocess.run(command, cwd=KERNEL, env=environment,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            text=True, timeout=600)
    log.write_text(result.stdout + result.stderr)
    return result


def check(output):
    production = feature_contract()
    output.mkdir(parents=True, exist_ok=True)
    environment = dict(os.environ, HYPER_CONFIG=str(KERNEL / 'configs/qemu_aarch64_defconfig'),
                       CARGO_TARGET_DIR=str(output / 'cargo'))
    cargo = environment.get('CARGO', 'cargo')
    result = run([cargo, 'metadata', '--format-version=1', '--locked',
                  '--filter-platform', TARGET], environment, output / 'default-graph.json')
    if result.returncode:
        raise RuntimeError(f'default Cargo graph failed; see {output / "default-graph.json"}')
    graph = json.loads(result.stdout)
    packages = {package['id']: package['name'] for package in graph['packages']}
    for node in graph['resolve']['nodes']:
        if packages[node['id']] not in TEST_FEATURES:
            continue
        forbidden = {'kernel-test', *TEST_FEATURES[packages[node['id']]]}
        if enabled := forbidden.intersection(node['features']):
            raise RuntimeError(f'production defaults enabled kernel test features: {sorted(enabled)}')

    def compile_case(package, features, *, reject=False):
        label = package + '-' + ('-'.join(features) if features else 'production-minimal')
        logfile = output / (label + '.log')
        print(f'Kernel configuration: {label}', flush=True)
        command = [cargo, 'check', '--locked', '--offline', '--target', TARGET,
                   '--package', package, '--no-default-features']
        if features:
            command += ['--features', ','.join(features)]
        result = run(command, environment, logfile)
        diagnostics = result.stdout + result.stderr
        if reject:
            # Build-script admission must be the reason for failure. A Rust
            # source error cannot substitute for rejecting dangerous options.
            if (result.returncode == 0
                    or 'requires explicit `kernel-test`' not in diagnostics
                    or 'error[E' in diagnostics):
                raise RuntimeError(f'expected explicit kernel-test rejection; see {logfile}')
        elif result.returncode:
            raise RuntimeError(f'kernel feature compilation failed; see {logfile}')

    # Positive baselines establish that negative cases are not passing due to
    # missing targets, invalid architecture configuration or unavailable crates.
    compile_case('hyper', ())
    compile_case('hyper', ('kernel-test',))
    for feature in production:
        compile_case('hyper', (feature,))
    for package, selectors in TEST_FEATURES.items():
        for selector in selectors:
            compile_case(package, (selector,), reject=True)
            compile_case(package, ('kernel-test', selector))
    print('Kernel configuration matrix passed', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=KERNEL / 'target/ci/features')
    args = parser.parse_args()
    try:
        check(args.output.resolve())
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as error:
        parser.exit(1, f'kernel configurations: {error}\n')


if __name__ == '__main__':
    main()
