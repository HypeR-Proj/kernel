#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Check Make feature decisions against Cargo argument and alias semantics."""

import importlib.util
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import tomllib
import unittest

KERNEL = Path(__file__).resolve().parents[2]
TOOL = KERNEL / 'tools/kernel-features.py'
SPEC = importlib.util.spec_from_file_location('kernel_features', TOOL)
FEATURES = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FEATURES)
GRAPH = tomllib.loads((KERNEL / 'Cargo.toml').read_text())['features']


class FeatureTests(unittest.TestCase):
    def invoke(self, arguments):
        return subprocess.run([sys.executable, '-B', str(TOOL), *arguments],
                              capture_output=True, text=True)

    def make(self, target, *arguments, fragment=None, dry_run=False):
        with tempfile.NamedTemporaryFile(mode='w', suffix='.mk') as extra:
            extra.write(fragment or '')
            extra.flush()
            command = ['make', '--no-print-directory', '-s', '-f', 'Makefile', '-f', extra.name]
            if dry_run:
                command += ['-n', '-o', 'prepare-config']
            return subprocess.run([*command, target, *arguments], cwd=KERNEL,
                                  capture_output=True, text=True)

    def test_shell_argument_forms_select_the_same_features(self):
        forms = ['--features "runtime-symbols rtc"', '--features=runtime-symbols,rtc',
                 '-F "runtime-symbols,rtc"', '-Fruntime-symbols,rtc',
                 '--features runtime-symbols -F rtc']
        for flags in forms:
            with self.subTest(flags=flags):
                result = self.invoke(['--no-default-features', *shlex.split(flags)])
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.split(), ['rtc', 'runtime-symbols'])

    def test_default_and_all_features_expand_manifest_aliases(self):
        self.assertEqual(FEATURES.resolve(['--no-default-features'], GRAPH), [])
        default = FEATURES.resolve([], GRAPH)
        self.assertEqual(default, FEATURES.resolve(['--no-default-features', '-Fdefault'], GRAPH))
        self.assertIn('runtime-symbols', default)
        self.assertNotIn('kernel-test', default)
        self.assertEqual(FEATURES.resolve(['--no-default-features', '--all-features'], GRAPH),
                         sorted(GRAPH))
        smmu = FEATURES.resolve(['--no-default-features', '-Fkernel-smmuv3-test'], GRAPH)
        self.assertEqual(smmu, ['kernel-self-test', 'kernel-smmuv3-test'])

    def test_unknown_or_ambiguous_inputs_fail_explicitly(self):
        for flags in (['-Fmissing'], ['-Fhyper/runtime-symbols'], ['--release'], ['--help'], ['--features'],
                      ['--all-features', '-Fmissing']):
            with self.subTest(flags=flags):
                result = self.invoke(flags)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(result.stdout.strip())
                self.assertTrue(result.stderr.strip())

    def test_make_uses_resolved_features_for_image_and_validation(self):
        for kernel, flags in [('', '--features "runtime-symbols"'), ('', '-Fruntime-symbols'),
                              ('', '--features=runtime-symbols'), ('default', ''),
                              ('', '--all-features')]:
            with self.subTest(kernel=kernel, flags=flags):
                args = ['KERNEL_FEATURES=' + kernel, 'CARGO_FEATURES=' + flags]
                build = self.make('build', *args, dry_run=True)
                self.assertEqual(build.returncode, 0, build.stderr)
                self.assertIn('if [ "1" = 1 ]; then', build.stdout)
                verify = self.make('test-image', *args, dry_run=True)
                self.assertEqual(verify.returncode, 0, verify.stderr)
                self.assertIn('KERNEL_RUNTIME_SYMBOLS=1', verify.stdout)
                self.assertIn('KERNEL_SELF_TEST_IMAGE=' + str(int(flags == '--all-features')),
                              verify.stdout)
        disabled = self.make('build', 'KERNEL_FEATURES=', dry_run=True)
        self.assertEqual(disabled.returncode, 0, disabled.stderr)
        self.assertIn('if [ "0" = 1 ]; then', disabled.stdout)

    def test_target_specific_flags_reach_prerequisite_decisions(self):
        fragment = '''
__fixture: CARGO_FEATURES = --features "kernel-test kernel-smmuv3-test runtime-symbols"
__fixture: __decisions
__decisions:
	@printf '%s\\n' "$(KERNEL_ENABLED_FEATURES)" "$(KERNEL_RUNTIME_SYMBOLS)" "$(KERNEL_SELF_TEST_IMAGE)"
'''
        result = self.make('__fixture', 'KERNEL_FEATURES=', fragment=fragment)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            'kernel-self-test kernel-smmuv3-test kernel-test runtime-symbols', '1', '1'])

    def test_make_does_not_swallow_invalid_feature_arguments(self):
        for target in ('build', 'test-image'):
            result = self.make(target, 'KERNEL_FEATURES=', 'CARGO_FEATURES=--features missing',
                               dry_run=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('unknown kernel feature: missing', result.stderr)
            self.assertIn('invalid kernel feature selection', result.stderr)

    def test_clean_selects_only_the_requested_build_directory(self):
        selected = str(Path(tempfile.gettempdir()) / 'hyper-feature-clean-boundary')
        result = self.make('clean', 'KERNEL_BUILD_ROOT=' + selected, dry_run=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(shlex.split(result.stdout), ['CARGO_TARGET_DIR=' + selected, 'cargo', 'clean'])


if __name__ == '__main__':
    unittest.main()
