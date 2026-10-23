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
    '7Process34create_user_thread_with_scheduling',
    '24sys_process_builder_seal15ProcessServices',
    '36sys_memory_inspector_read_statistics15ProcessServices',
    '6loader11load_native',
    '6loader20create_address_space',
    '6loader21prepare_address_space',
    '32sys_pending_virtual_machine_seal15ProcessServices',
    '9VmBuilder17prepare_boot_vcpu',
    '13VcpuExecution13try_installed',
    '12CpuScheduler15local_selection',
    '12CpuScheduler12choose_ready',
    'ready_index7rebuild19ReadyQueueAuthority',
    'ready_index7rebuild24LocalReadyQueueAuthority',
    'ready_index7balance19ReadyQueueAuthority',
    'ready_index7balance24LocalReadyQueueAuthority',
    'ready_index4Tree6remove19ReadyQueueAuthority',
    'ready_index4Tree6remove24LocalReadyQueueAuthority',
    '18NativeAddressSpace21resolve_private_write',
    '18NativeAddressSpace19populate_file_range',
    '30sys_capability_channel_receive15ProcessServices',
    '25PreparedCapabilityReceive7try_new',
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

    def test_dispatch_candidate_array_regression_is_rejected(self):
        symbol = '12CpuScheduler15local_selection'
        self.functions[symbol] = function(symbol, 'stp x29, x30, [sp, #-0x60]!\n'
                                          'sub sp, sp, #0xc30\n'
                                          'str xzr, [sp]\nbl 0x20000\nret')
        with self.assertRaisesRegex(ValueError, 'local_selection: entry frame 3216 exceeds'):
            self.check()

    def test_worker_creation_cannot_retain_register_image_scratch(self):
        symbol = '7Process34create_user_thread_with_scheduling'
        self.functions[symbol] = function(symbol, 'sub sp, sp, #3808\nret')
        with self.assertRaisesRegex(ValueError, 'entry frame 3808 exceeds'):
            self.check()

    def test_dispatch_bounds_apply_to_self_test_images(self):
        symbol = '12CpuScheduler12choose_ready'
        self.functions[symbol] = function(symbol, 'sub sp, sp, #0x410\nret')
        with self.assertRaisesRegex(ValueError, 'choose_ready: entry frame 1040 exceeds'):
            self.check(self_test=True)

    def test_avl_link_access_cannot_copy_the_full_runtime_snapshot(self):
        symbol = 'ready_index7rebuild19ReadyQueueAuthority'
        self.functions[symbol] = function(symbol, 'sub sp, sp, #0x140\n'
                                          'stp x29, x30, [sp, #0xe0]\nbl 0x20000\nret')
        with self.assertRaisesRegex(ValueError, 'rebuild.*entry frame 320 exceeds'):
            self.check()

    def test_mapping_prepare_scratch_cannot_survive_into_commit(self):
        for symbol, size in (
                ('18NativeAddressSpace21resolve_private_write', 2688),
                ('18NativeAddressSpace19populate_file_range', 2768)):
            with self.subTest(symbol=symbol):
                original = self.functions[symbol]
                self.functions[symbol] = function(
                    symbol, f'sub sp, sp, #{size}\nbl 0x20000\nret')
                with self.assertRaisesRegex(ValueError,
                                            f'entry frame {size} exceeds'):
                    self.check()
                self.functions[symbol] = original

    def test_queue_mutation_cannot_restore_stack_allocated_ancestor_paths(self):
        for authority in ('19ReadyQueueAuthority', '24LocalReadyQueueAuthority'):
            symbol = 'ready_index4Tree6remove' + authority
            with self.subTest(authority=authority):
                original = self.functions[symbol]
                self.functions[symbol] = function(symbol, 'sub sp, sp, #560\nret')
                with self.assertRaisesRegex(ValueError, 'entry frame 560 exceeds'):
                    self.check()
                self.functions[symbol] = original

    def test_receive_registration_scratch_cannot_survive_into_output_admission(self):
        symbol = '30sys_capability_channel_receive15ProcessServices'
        self.functions[symbol] = function(
            symbol, 'stp x29, x30, [sp, #-0x60]!\n'
            'sub sp, sp, #0xfc0\nbl 0x20000\nret')
        with self.assertRaisesRegex(ValueError, 'entry frame 4128 exceeds'):
            self.check()

    def test_outlining_receive_output_preparation_cannot_hide_live_stack_growth(self):
        symbol = '30sys_capability_channel_receive15ProcessServices'
        self.functions[symbol] = function(symbol, 'sub sp, sp, #2464\nret')
        self.functions['target'] = function(
            '23CapabilityReceiveTarget7prepare', 'sub sp, sp, #2912\nret')
        with self.assertRaisesRegex(ValueError, 'live frames 5376 exceed 4096'):
            self.check()

    def test_unrecognized_optional_receive_preparation_frame_is_rejected(self):
        self.functions['target'] = function('23CapabilityReceiveTarget7prepare', 'ret')
        with self.assertRaisesRegex(ValueError, 'unrecognized capability receive target'):
            self.check()


if __name__ == '__main__':
    unittest.main()
