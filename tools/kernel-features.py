#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Resolve kernel Make feature arguments using the root Cargo feature graph."""

import argparse
from pathlib import Path
import sys
import tomllib

MANIFEST = Path(__file__).resolve().parents[1] / 'Cargo.toml'


def resolve(arguments, features):
    """Accept feature flags after shell expansion, rejecting other Cargo options."""
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False, add_help=False)
    parser.add_argument('--features', '-F', action='append', default=[])
    parser.add_argument('--no-default-features', action='store_true')
    parser.add_argument('--all-features', action='store_true')
    args = parser.parse_args(arguments)
    selected = set(features) if args.all_features else set()
    if not args.no_default_features and 'default' in features:
        selected.add('default')
    for group in args.features:
        selected.update(group.replace(',', ' ').split())
    for name in selected:
        if '/' in name:
            raise ValueError('kernel Make accepts root feature names only; '
                             f'use Cargo directly for a crate-qualified feature: {name}')
        if name not in features:
            raise ValueError(f'unknown kernel feature: {name}')
    pending = list(selected)
    while pending:
        for dependency in features[pending.pop()]:
            # Qualified edges forward a root feature into a mechanism crate.
            # They do not enable another feature on the kernel binary itself.
            if '/' in dependency or dependency.startswith('dep:'):
                continue
            if dependency not in features:
                raise ValueError(f'unknown local kernel feature dependency: {dependency}')
            if dependency not in selected:
                selected.add(dependency)
                pending.append(dependency)
    return sorted(selected)


def main():
    try:
        features = tomllib.loads(MANIFEST.read_text())['features']
        print(' '.join(resolve(sys.argv[1:], features)))
    except (OSError, ValueError) as error:
        sys.exit(f'kernel features: {error}')


if __name__ == '__main__':
    main()
