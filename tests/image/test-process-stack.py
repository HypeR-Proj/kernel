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
    '26sys_process_builder_create15ProcessServices',
    '14ProcessBuilder15try_publication',
    '14ProcessBuilder15allocate_loaded',
    '25copy_executable_for_child',
    '18ExecutableSnapshot4read18FilesystemInstance',
    'executable13parser_buffer',
    '26sys_filesystem_mount_ramfs15ProcessServices',
    '18FilesystemInstance15try_empty_ramfs',
    '20sys_filesystem_mount15ProcessServices',
    '18FilesystemInstance15try_from_remote',
    '9Transport7prepare',
    '7Process24reserve_handle_batch_for',
    '7Process26prepare_table_storage_plan',
    '7Process28prepare_handle_batch_scratch',
    '7Process28reserve_table_storage_charge',
    '7Process31finish_handle_batch_reservation',
    '24sys_process_builder_seal15ProcessServices',
    '14ProcessBuilder11finish_seal',
    '25sys_process_builder_start15ProcessServices',
    '36sys_memory_inspector_read_statistics15ProcessServices',
    '6loader11load_native',
    '9bootstrap5Image5parse',
    '6loader20create_address_space',
    '23TranslationEpochSegment7try_new',
    '6loader21prepare_address_space',
    '32sys_pending_virtual_machine_seal15ProcessServices',
    '9VmBuilder17prepare_boot_vcpu',
    '13VcpuExecution13try_installed',
    '12CpuScheduler15local_selection',
    '12CpuScheduler12choose_ready',
    'NC24with_cpu_schedule_stored16balance_ready_in0E0',
    '9Scheduler21select_balance_target',
    '9Scheduler14migrate_thread',
    '9Scheduler17request_migration',
    '9Scheduler17move_ready_thread',
    'ready_index7rebuild19ReadyQueueAuthority',
    'ready_index7rebuild24LocalReadyQueueAuthority',
    'ready_index7balance19ReadyQueueAuthority',
    'ready_index7balance24LocalReadyQueueAuthority',
    'ready_index4Tree6remove19ReadyQueueAuthority',
    'ready_index4Tree6remove24LocalReadyQueueAuthority',
    '18NativeAddressSpace21resolve_private_write',
    '18NativeAddressSpace19populate_file_range',
    '17KernelPageBackend13DomainAccountE26prepare_copy_from_snapshot',
    '17KernelPageBackend13DomainAccountE18prepare_user_write',
    '18NativeAddressSpace28prepare_private_write_change',
    'vfs6remote9transport15decode_response',
    'vfs7resolve4walk',
    'vfs7resolve12prepare_walk',
    'vfs7resolve13restore_start',
    'vfs7resolve15walk_components',
    'vfs7resolve11finish_walk',
    '9Transport8exchange',
    '9Transport13wait_response',
    'wait19prepare_signal_wait',
    '30sys_capability_channel_receive15ProcessServices',
    '25PreparedCapabilityReceive7try_new',
    '31sys_capability_channel_try_send15ProcessServices',
    '22PreparedCapabilitySend14copy_from_user',
    '26send_prepared_capabilities',
    '9Transport12read_scatter',
    'transfer14read_unaligned',
    'NCNv7service8map_file0',
    '7service20map_loaded_file_view',
    'scatter11BlockAccess12read_scatter',
    '10BlockQueue10wait_space11BlockAccess12read_scatter',
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

    def test_loader_snapshot_cannot_retain_builder_construction_scratch(self):
        symbol = '14ProcessBuilder15try_publication'
        self.functions[symbol] = function(symbol, 'sub sp, sp, #1200\nret')
        with self.assertRaisesRegex(ValueError, 'entry frame 1200 exceeds'):
            self.check()

    def test_loader_io_cannot_retain_preparation_scratch(self):
        for symbol, frame in (
                ('26sys_process_builder_create15ProcessServices', 1872),
                ('18ExecutableSnapshot4read18FilesystemInstance', 720),
                ('9Transport12read_scatter', 1232),
                ('NCNv7service8map_file0', 1088),
                ('26sys_filesystem_mount_ramfs15ProcessServices', 1328),
                ('20sys_filesystem_mount15ProcessServices', 2176),
                ('24sys_process_builder_seal15ProcessServices', 1424),
                ('6loader11load_native', 1520),
                ('vfs7resolve4walk', 912),
                ('vfs7resolve15walk_components', 944),
                ('9Transport8exchange', 784),
                ('9Transport13wait_response', 672),
                ('7Process24reserve_handle_batch_for', 1904),
                ('7Process26prepare_table_storage_plan', 880),
                ('scatter11BlockAccess12read_scatter', 1440)):
            with self.subTest(symbol=symbol):
                original = self.functions[symbol]
                self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
                with self.assertRaisesRegex(ValueError, f'entry frame {frame} exceeds'):
                    self.check()
                self.functions[symbol] = original

    def test_scatter_wait_bounds_the_combined_live_frames(self):
        for symbol, frame in (('scatter11BlockAccess12read_scatter', 768),
                              ('10BlockQueue10wait_space11BlockAccess12read_scatter', 1280)):
            self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
        with self.assertRaisesRegex(ValueError, 'scatter admission wait: live frames 2048'):
            self.check()

    def test_builder_construction_bounds_the_combined_live_frames(self):
        for symbol, frame in (('26sys_process_builder_create15ProcessServices', 1024),
                              ('14ProcessBuilder15try_publication', 512),
                              ('14ProcessBuilder15allocate_loaded', 1536)):
            self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
        with self.assertRaisesRegex(ValueError, 'process creation .*live frames 3072'):
            self.check()

    def test_unaligned_read_bounds_the_combined_live_frames(self):
        for symbol, frame in (('9Transport12read_scatter', 512),
                              ('transfer14read_unaligned', 512)):
            self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
        with self.assertRaisesRegex(ValueError, 'unaligned extent read: live frames 1024'):
            self.check()

    def test_file_mapping_bounds_the_combined_live_frames(self):
        for symbol, frame in (('NCNv7service8map_file0', 1024),
                              ('7service20map_loaded_file_view', 768)):
            self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
        with self.assertRaisesRegex(ValueError, 'file mapping publication: live frames 1792'):
            self.check()

    def test_rpc_wait_cannot_hide_live_frame_growth(self):
        for symbol, frame in (('9Transport8exchange', 384),
                              ('9Transport13wait_response', 512)):
            self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
        with self.assertRaisesRegex(ValueError, 'filesystem RPC wait: live frames 896'):
            self.check()

    def test_handle_table_growth_cannot_hide_live_frame_growth(self):
        for symbol, frame in (('7Process24reserve_handle_batch_for', 1536),
                              ('7Process26prepare_table_storage_plan', 832)):
            self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
        with self.assertRaisesRegex(ValueError, 'handle batch .*live frames 2368'):
            self.check()

    def test_loader_mapping_bounds_the_combined_live_frames(self):
        for symbol, frame in (('24sys_process_builder_seal15ProcessServices', 1280),
                              ('6loader11load_native', 832),
                              ('6loader21prepare_address_space', 768)):
            self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
        with self.assertRaisesRegex(ValueError, 'process seal .*live frames 2880'):
            self.check()

    def test_irq_tail_migration_cannot_retain_full_observations(self):
        for symbol, frame in (('9Scheduler14migrate_thread', 320),
                              ('9Scheduler17request_migration', 496)):
            with self.subTest(symbol=symbol):
                original = self.functions[symbol]
                self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
                with self.assertRaisesRegex(ValueError, f'entry frame {frame} exceeds'):
                    self.check()
                self.functions[symbol] = original

    def test_irq_tail_migration_bounds_the_combined_live_frames(self):
        for symbol, frame in (('NC24with_cpu_schedule_stored16balance_ready_in0E0', 192),
                              ('9Scheduler14migrate_thread', 256),
                              ('9Scheduler17request_migration', 256),
                              ('9Scheduler17move_ready_thread', 320)):
            self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
        with self.assertRaisesRegex(ValueError, 'balance migration: live frames 1024'):
            self.check()

    def test_seal_publication_bounds_the_combined_live_frames(self):
        for symbol, frame in (('24sys_process_builder_seal15ProcessServices', 1280),
                              ('14ProcessBuilder11finish_seal', 1024)):
            self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
        with self.assertRaisesRegex(ValueError, 'process seal publication: live frames 2304'):
            self.check()

    def test_rpc_preparation_cannot_hide_live_frame_growth(self):
        for symbol, frame in (('9Transport8exchange', 256),
                              ('9Transport13wait_response', 512),
                              ('wait19prepare_signal_wait', 640)):
            self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
        with self.assertRaisesRegex(ValueError, 'filesystem RPC wait preparation: live frames 1408'):
            self.check()

    def test_path_result_assembly_cannot_hide_live_frame_growth(self):
        for symbol, frame in (('vfs7resolve4walk', 256),
                              ('vfs7resolve15walk_components', 768),
                              ('vfs7resolve11finish_walk', 512)):
            self.functions[symbol] = function(symbol, f'sub sp, sp, #{frame}\nret')
        with self.assertRaisesRegex(ValueError, 'VFS result assembly: live frames 1536'):
            self.check()

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

    def test_vfs_lookup_cannot_retain_all_preparation_scratch(self):
        symbol = 'vfs7resolve4walk'
        self.functions[symbol] = function(symbol, 'sub sp, sp, #2528\nret')
        with self.assertRaisesRegex(ValueError, 'entry frame 2528 exceeds'):
            self.check()

    def test_vfs_outlining_cannot_hide_live_frame_growth(self):
        for phase, frame in (('15walk_components', 896), ('13restore_start', 1664)):
            with self.subTest(phase=phase):
                self.setUp()
                self.functions['vfs7resolve4walk'] = function(
                    'vfs7resolve4walk', 'sub sp, sp, #256\nret')
                self.functions['vfs7resolve' + phase] = function(
                    'vfs7resolve' + phase, f'sub sp, sp, #{frame}\nret')
                with self.assertRaisesRegex(ValueError, 'VFS .*live frames'):
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

    def test_extent_plan_cannot_return_to_nested_read_stack(self):
        symbol = '9Transport12read_scatter'
        self.functions[symbol] = function(symbol, 'sub sp, sp, #2288\nret')
        with self.assertRaisesRegex(ValueError, 'read_scatter: entry frame 2288 exceeds'):
            self.check()

    def test_worker_creation_cannot_retain_register_image_scratch(self):
        symbol = '7Process34create_user_thread_with_scheduling'
        self.functions[symbol] = function(symbol, 'sub sp, sp, #3808\nret')
        with self.assertRaisesRegex(ValueError, 'entry frame 3808 exceeds'):
            self.check()

    def test_process_start_cannot_retain_duplicate_transaction_storage(self):
        symbol = '25sys_process_builder_start15ProcessServices'
        self.functions[symbol] = function(symbol, 'sub sp, sp, #1, lsl #12\n'
                                          'sub sp, sp, #64\nbl 0x20000\nret')
        with self.assertRaisesRegex(ValueError, 'entry frame 4160 exceeds'):
            self.check()

    def test_translation_storage_cannot_return_to_stack_construction(self):
        for symbol, size in [('6loader20create_address_space', 5008),
                             ('23TranslationEpochSegment7try_new', 4144)]:
            with self.subTest(symbol=symbol):
                original = self.functions[symbol]
                self.functions[symbol] = function(symbol, f'sub sp, sp, #{size}\nret')
                with self.assertRaisesRegex(ValueError, f'entry frame {size} exceeds'):
                    self.check()
                self.functions[symbol] = original

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

    def test_mapping_cursors_cannot_restore_maximum_height_arrays(self):
        for symbol, size in (
                ('17KernelPageBackend13DomainAccountE26prepare_copy_from_snapshot', 2560),
                ('18NativeAddressSpace28prepare_private_write_change', 2528)):
            with self.subTest(symbol=symbol):
                original = self.functions[symbol]
                self.functions[symbol] = function(symbol, f'sub sp, sp, #{size}\nret')
                with self.assertRaisesRegex(ValueError, f'entry frame {size} exceeds'):
                    self.check()
                self.functions[symbol] = original

    def test_user_write_cannot_hide_combined_copy_admission_growth(self):
        for method, size in [('26prepare_copy_from_snapshot', 1024),
                             ('18prepare_user_write', 1536)]:
            symbol = '17KernelPageBackend13DomainAccountE' + method
            self.functions[symbol] = function(symbol, f'sub sp, sp, #{size}\nret')
        with self.assertRaisesRegex(ValueError, 'user write admission: live frames 2560'):
            self.check()

    def test_filesystem_response_cannot_retain_owned_name_copies(self):
        symbol = 'vfs6remote9transport15decode_response'
        self.functions[symbol] = function(symbol, 'sub sp, sp, #2336\nret')
        with self.assertRaisesRegex(ValueError, 'entry frame 2336 exceeds'):
            self.check()

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

    def test_send_delivery_scratch_cannot_survive_into_input_admission(self):
        symbol = '31sys_capability_channel_try_send15ProcessServices'
        self.functions[symbol] = function(
            symbol, 'stp x29, x30, [sp, #-0x60]!\n'
            'sub sp, sp, #0xfc0\nbl 0x20000\nret')
        with self.assertRaisesRegex(ValueError, 'entry frame 4128 exceeds'):
            self.check()

    def test_outlining_send_input_cannot_hide_live_stack_growth(self):
        caller = '31sys_capability_channel_try_send15ProcessServices'
        preparation = '22PreparedCapabilitySend14copy_from_user'
        self.functions[caller] = function(caller, 'sub sp, sp, #640\nret')
        self.functions[preparation] = function(preparation, 'sub sp, sp, #960\nret')
        with self.assertRaisesRegex(ValueError, 'live frames 1600 exceed 1536'):
            self.check()


if __name__ == '__main__':
    unittest.main()
