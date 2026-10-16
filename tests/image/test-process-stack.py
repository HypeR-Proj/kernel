#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Keep production stack bounds enforced in kernels with self-test symbols."""

import contextlib
import importlib.util
import io
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    'process_stack', Path(__file__).with_name('check-process-stack.py'))
process_stack = importlib.util.module_from_spec(spec)
spec.loader.exec_module(process_stack)

SYMBOLS = (
    '15PreparedProcess7try_new',
    '7Process17finish_retirement',
    '24sys_process_builder_seal15ProcessServices',
    '36sys_memory_inspector_read_statistics15ProcessServices',
    '6loader11load_native',
    '6loader20create_address_space',
    '6loader21prepare_address_space',
    '32sys_pending_virtual_machine_seal15ProcessServices',
    '9VmBuilder17prepare_boot_vcpu',
    '13VcpuExecution13try_installed',
)


def function(symbol, instructions='sub sp, sp, #0x60\nret'):
    lines = [f'0000000000010000 <_R{symbol}>:']
    lines.extend(f'   {0x10000 + index * 4:x}: 00000000    {operation}'
                 for index, operation in enumerate(instructions.splitlines()))
    return '\n'.join(lines) + '\n'


class ProcessStackTests(unittest.TestCase):
    def setUp(self):
        self.functions = {symbol: function(symbol) for symbol in SYMBOLS}

    def check(self, *, self_test=False):
        with contextlib.redirect_stdout(io.StringIO()) as output:
            process_stack.check(''.join(self.functions.values()), self_test=self_test)
        return output.getvalue()

    def test_production_frames_are_checked(self):
        self.assertEqual(len(self.check().splitlines()), len(SYMBOLS))

    def test_self_test_services_do_not_replace_real_syscall(self):
        del self.functions['6loader11load_native']
        self.functions['test-service'] = function(
            '24sys_process_builder_seal17RejectingServices', 'ret')
        self.assertEqual(len(self.check(self_test=True).splitlines()), len(SYMBOLS) - 1)
        del self.functions['24sys_process_builder_seal15ProcessServices']
        with self.assertRaisesRegex(ValueError, 'ProcessServices.*found 0'):
            self.check(self_test=True)

    def test_production_requires_bootstrap_loader(self):
        del self.functions['6loader11load_native']
        with self.assertRaisesRegex(ValueError, 'load_native.*found 0'):
            self.check()

    def test_self_test_retained_loader_still_has_a_budget(self):
        self.functions['6loader11load_native'] = function(
            '6loader11load_native', 'sub sp, sp, #3, lsl #12\nret')
        with self.assertRaisesRegex(ValueError, 'entry frame 12288 exceeds'):
            self.check(self_test=True)

    def test_self_test_requires_other_frames(self):
        del self.functions['6loader20create_address_space']
        with self.assertRaisesRegex(ValueError, 'create_address_space.*found 0'):
            self.check(self_test=True)

    def test_ambiguous_real_syscall_fails(self):
        self.functions['duplicate'] = function(
            '24sys_process_builder_seal15ProcessServicesOtherInstantiation')
        with self.assertRaisesRegex(ValueError, 'ProcessServices.*found 2'):
            self.check()

    def test_split_prologue_reservations_are_accumulated(self):
        symbol = '15PreparedProcess7try_new'
        self.functions[symbol] = function(symbol, 'stp x29, x30, [sp, #-0x60]!\n'
                                          'sub sp, sp, #0x800\nbl 0x20000\nret')
        with self.assertRaisesRegex(ValueError, 'entry frame 2144 exceeds'):
            self.check()

    def test_unrecognized_frame_cannot_silently_pass(self):
        symbol = '15PreparedProcess7try_new'
        self.functions[symbol] = function(symbol, 'ret')
        with self.assertRaisesRegex(ValueError, 'entry frame 0 exceeds'):
            self.check()


if __name__ == '__main__':
    unittest.main()
