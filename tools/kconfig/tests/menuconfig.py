#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Exercise the real curses frontend in a PTY and the shared Rust resolver."""

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest

from terminal import Terminal

BINARY = str(Path(sys.argv.pop(1)).resolve())
KERNEL = Path(__file__).resolve().parents[3]


class MenuTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='hyper-menu-test-')
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.config = self.directory / 'config'
        self.config.write_text('CONFIG_ARCH_AARCH64=y\n')
        self.backend()

    def backend(self, *arguments, success=True):
        result = subprocess.run([BINARY, 'menuconfig-state', str(self.config), *arguments],
                                cwd=KERNEL, capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode == 0, success, result.stderr)
        return json.loads(result.stdout) if success else result.stderr

    def terminal(self, size=(24, 100), command=None):
        terminal = Terminal(command or [BINARY, 'menuconfig', str(self.config), str(self.config)], size, cwd=KERNEL)
        self.addCleanup(terminal.close)
        terminal.expect(b'Main menu' if size[0] >= 12 else b'Resize terminal')
        return terminal

    def test_architecture_change_hides_and_rejects_kaslr(self):
        for arch in ('ARCH_RISCV64', 'ARCH_X86_64'):
            state = self.backend('ARCH', arch)
            self.assertFalse(any(entry['name'] == 'RANDOMIZE_BASE' for entry in state['entries']))
            before = self.config.read_bytes()
            self.assertIn('unavailable', self.backend('RANDOMIZE_BASE', 'y', success=False))
            self.assertEqual(self.config.read_bytes(), before)
            self.assertNotIn('CONFIG_ARM64_', before.decode())
        state = self.backend('ARCH', 'ARCH_AARCH64')
        self.assertTrue(any(entry['name'] == 'RANDOMIZE_BASE' for entry in state['entries']))

    def test_invalid_values_and_injection_preserve_working_copy(self):
        before = self.config.read_bytes()
        for name, value in [('KERNEL_STACK_SIZE_KB', '24'), ('ARM64_PA_BITS', '41'),
                            ('LOG_LINE_MAX', '512\nCONFIG_MAX_CPUS=256'), ('ARCH', 'ARCH_OTHER')]:
            self.backend(name, value, success=False)
            self.assertEqual(self.config.read_bytes(), before)

    def test_keyboard_architecture_selection_and_save(self):
        terminal = self.terminal()
        # Category -> architecture choice -> RISC-V using a real Down key.
        terminal.send(b'\n\n\x1bOB\nsqq')
        terminal.finish()
        text = self.config.read_text()
        self.assertIn('CONFIG_ARCH_RISCV64=y', text)
        self.assertNotIn('RANDOMIZE_BASE', text)
        self.assertNotIn('CONFIG_ARM64_', text)

    def test_search_toggle_and_discard(self):
        before = self.config.read_bytes()
        terminal = self.terminal()
        terminal.send(b'/CONFIG_RANDOMIZE_BASE\nN')
        terminal.expect(b'not yet saved')
        terminal.send(b'QQd')
        terminal.finish()
        self.assertEqual(self.config.read_bytes(), before)

    def test_bad_stack_size_reports_error_and_can_be_corrected(self):
        terminal = self.terminal()
        terminal.send(b'jj\n\n\x1524\n')
        terminal.expect(b'Invalid value; setting unchanged')
        terminal.send(b'\n\n\x1516\nsqq')
        terminal.finish()
        self.assertIn('CONFIG_KERNEL_STACK_SIZE_KB=16', self.config.read_text())

    def test_cancellation_does_not_create_missing_configuration(self):
        self.config.unlink()
        terminal = self.terminal()
        terminal.send(b'qd')
        terminal.finish()
        self.assertFalse(self.config.exists())

    def test_small_terminal_cancels_and_restores_attributes(self):
        before = self.config.read_bytes()
        terminal = self.terminal(size=(8, 40))
        terminal.send(b'q')
        terminal.expect(b'unsaved changes discarded')
        terminal.process.wait(timeout=5)
        self.assertTrue(terminal.restored())
        self.assertEqual(self.config.read_bytes(), before)

    def test_interrupt_discards_edits_and_restores_terminal(self):
        before = self.config.read_bytes()
        terminal = self.terminal()
        terminal.send(b'/RANDOMIZE_BASE\n ')
        terminal.expect(b'not yet saved')
        os.killpg(terminal.process.pid, signal.SIGINT)
        terminal.expect(b'unsaved changes discarded')
        terminal.process.wait(timeout=5)
        self.assertTrue(terminal.restored())
        self.assertEqual(self.config.read_bytes(), before)

    def test_save_keeps_symlink_and_permissions(self):
        target = self.directory / 'target.config'
        self.config.rename(target)
        target.chmod(0o640)
        self.config.symlink_to(target)
        terminal = self.terminal()
        terminal.send(b'/LOG_LINE_MAX\n\n\x151024\nsqq')
        terminal.finish()
        self.assertTrue(self.config.is_symlink())
        self.assertEqual(target.stat().st_mode & 0o777, 0o640)
        self.assertIn('CONFIG_LOG_LINE_MAX=1024', target.read_text())

    def test_make_menuconfig_does_not_reset_existing_architecture(self):
        self.backend('ARCH', 'ARCH_RISCV64')
        before = self.config.read_bytes()
        terminal = self.terminal(command=['make', 'menuconfig', 'ARCH=aarch64',
                                         'CONFIG_FILE=' + str(self.config)])
        terminal.send(b'q')
        terminal.finish()
        self.assertEqual(self.config.read_bytes(), before)

    def test_noninteractive_invocation_does_not_touch_output(self):
        self.config.unlink()
        result = subprocess.run([BINARY, 'menuconfig', str(self.config), str(self.config)],
                                cwd=KERNEL, capture_output=True, text=True, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('interactive terminal', result.stderr)
        self.assertFalse(self.config.exists())


if __name__ == '__main__':
    unittest.main()
