#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Check Make feature decisions against Cargo argument and alias semantics."""

import importlib.util
import json
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
MANIFEST = tomllib.loads((KERNEL / 'Cargo.toml').read_text())
GRAPH = MANIFEST['features']


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
            if not any(arg.startswith('CONFIG_FILE=') for arg in arguments):
                architecture = next((arg.removeprefix('ARCH=') for arg in arguments
                                     if arg.startswith('ARCH=')), 'aarch64')
                command += ['CONFIG_FILE=' + str(KERNEL / f'configs/qemu_{architecture}_defconfig')]
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

    @classmethod
    def setUpClass(cls):
        host = subprocess.check_output(['rustc', '-vV'], text=True)
        host = next(line.removeprefix('host: ') for line in host.splitlines()
                    if line.startswith('host: '))
        cls.configurator = ['cargo', 'run', '--quiet', '--locked', '--offline',
                            '--manifest-path', 'tools/kconfig/Cargo.toml', '--target', host, '--']

    def configuration(self, *arguments):
        return subprocess.run([*self.configurator, *map(str, arguments)], cwd=KERNEL,
                              text=True, capture_output=True)

    def test_menu_covers_every_root_feature_and_architecture_restriction(self):
        result = self.configuration('--schema', KERNEL / 'Kconfig', 'describe')
        self.assertEqual(result.returncode, 0, result.stderr)
        entries = json.loads(result.stdout)['entries']
        menu = {entry['name'].removeprefix('CONFIG_FEATURE_').lower().replace('_', '-'): entry
                for entry in entries if entry['name'].startswith('CONFIG_FEATURE_')}
        self.assertEqual(set(menu), set(GRAPH) - {'default'})
        policy = MANIFEST['package']['metadata']['hyper']
        for name, entry in menu.items():
            self.assertEqual(entry['kind'], 'bool')
            self.assertTrue(entry['prompt'])
            dependencies = {item['name'] for item in entry['dependencies']}
            if name not in policy['production-features']:
                # Children of the disabled umbrella have no resolved value.
                self.assertIn(entry['default'], ('n', ''))
                if name != 'kernel-test':
                    self.assertIn('CONFIG_FEATURE_KERNEL_TEST', dependencies)
            if name in policy['feature-architectures']:
                self.assertIn('CONFIG_ARCH_AARCH64', dependencies)
            for edge in GRAPH[name]:
                if '/' not in edge and not edge.startswith('dep:'):
                    self.assertIn('CONFIG_FEATURE_' + edge.upper().replace('-', '_'), dependencies)

    def test_make_defaults_follow_kconfig_and_architecture_availability(self):
        fragment = '__defaults:\n\t@printf "%s\\n" "$(KERNEL_FEATURES)"\n'
        for architecture in ('aarch64', 'riscv64', 'x86_64'):
            result = self.make('__defaults', 'ARCH=' + architecture, fragment=fragment)
            self.assertEqual(result.returncode, 0, result.stderr)
            expected = sorted(set(FEATURES.resolve([], GRAPH)) - {'default'})
            if architecture != 'aarch64':
                expected.remove('crash-console')
            self.assertEqual(result.stdout.split(), expected)

    def test_saved_configuration_controls_cargo_and_symbol_embedding(self):
        with tempfile.TemporaryDirectory(prefix='kernel feature config ') as directory:
            path = Path(directory) / 'saved.config'
            path.write_text('CONFIG_ARCH_AARCH64=y\n# CONFIG_FEATURE_RTC is not set\n'
                            '# CONFIG_FEATURE_RUNTIME_SYMBOLS is not set\n'
                            '# CONFIG_FEATURE_CRASH_CONSOLE is not set\n'
                            'CONFIG_FEATURE_KERNEL_TRACE=y\nCONFIG_FEATURE_PMU_PROFILING=y\n')
            original = path.read_bytes()
            result = self.make('build', 'CONFIG_FILE=' + str(path), dry_run=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('--features "kernel-trace pmu-profiling"', result.stdout)
            self.assertIn('if [ "0" = 1 ]; then', result.stdout)
            path.write_text(path.read_text() + 'CONFIG_FEATURE_KERNEL_TEST=y\n'
                            'CONFIG_FEATURE_KERNEL_SELF_TEST=y\nCONFIG_FEATURE_KERNEL_SMMUV3_TEST=y\n')
            result = self.make('test-image', 'CONFIG_FILE=' + str(path), dry_run=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('KERNEL_SELF_TEST_IMAGE=1', result.stdout)
            # An explicit fixture override still replaces saved feature flags.
            result = self.make('build', 'CONFIG_FILE=' + str(path), 'KERNEL_FEATURES=', dry_run=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn('--features ', result.stdout)
            path.write_bytes(original)
            result = self.make('build', 'CONFIG_FILE=' + str(path), dry_run=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(path.read_bytes(), original)

    def test_invalid_or_missing_feature_configuration_is_not_ignored(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'saved.config'
            for value in (None, 'CONFIG_ARCH_AARCH64=y\nCONFIG_FEATURE_IO_DMA_REVOKE_TEST=y\n',
                          'CONFIG_ARCH_RISCV64=y\nCONFIG_FEATURE_PMU_PROFILING=y\n'):
                if value is not None:
                    path.write_text(value)
                result = self.make('build', 'CONFIG_FILE=' + str(path), dry_run=True)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn('invalid kernel feature selection', result.stderr)

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
