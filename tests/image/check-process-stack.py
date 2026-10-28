#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Bound AArch64 construction and dispatch frames in the optimized ELF disassembly.

This guards the large by-value construction/retirement regression. It is not
a whole-call-graph stack bound; IRQ-tail headroom also needs runtime testing.
"""

import os
import re
import sys
from pathlib import Path


def entry_frame(body):
    size = 0
    for line in body.splitlines():
        instruction = re.match(r'\s*[0-9a-f]+:\s+[0-9a-f]+\s+(.*)', line)
        if not instruction:
            continue
        operation = instruction[1].split('//', 1)[0].strip()
        # Entry-frame allocation precedes the first control transfer.
        if re.match(r'(?:b(?:\.[a-z]+)?|bl|blr|br|ret|cbz|cbnz|tbz|tbnz)\s', operation):
            break
        reserve = re.fullmatch(r'sub\s+sp,\s*sp,\s*#(0x[0-9a-f]+|\d+)'
                               r'(?:,\s*lsl\s*#(\d+))?', operation)
        if reserve:
            size += int(reserve[1], 0) << int(reserve[2] or '0')
        push = re.search(r'\[sp,\s*#-(0x[0-9a-f]+|\d+)\]!', operation)
        if push:
            size += int(push[1], 0)
    return size


def check(disassembly, *, self_test=False):
    budgets = {
        '15PreparedProcess7try_new': 2048,
        '7Process17finish_retirement': 2048,
        # Worker creation must release architectural register-image scratch
        # before scheduler admission and its possible preemption/retirement.
        '7Process34create_user_thread_with_scheduling': 1024,
        # Snapshot I/O precedes builder construction. Do not retain BuilderPlan
        # scratch below filesystem/block reads and IRQ-tail load balancing.
        '26sys_process_builder_create.*15ProcessServices': 1024,
        '14ProcessBuilder15try_publication': 512,
        '14ProcessBuilder15allocate_loaded': 1536,
        '25copy_executable_for_child': 1536,
        '18ExecutableSnapshot4read.*18FilesystemInstance': 512,
        'executable13parser_buffer': 768,
        # RAM backend construction retires before resolving the mount point,
        # which may traverse a remote filesystem and enter IRQ-tail scheduling.
        '26sys_filesystem_mount_ramfs.*15ProcessServices': 512,
        '18FilesystemInstance15try_empty_ramfs': 1536,
        '20sys_filesystem_mount.*15ProcessServices': 1024,
        '18FilesystemInstance15try_from_remote': 1664,
        '9Transport7prepare': 1024,
        # Child startup and capability transfer can grow handle tables under
        # allocator/IRQ paths. Keep scratch admission and final records separate.
        '7Process24reserve_handle_batch_for': 1536,
        '7Process26prepare_table_storage_plan': 832,
        '7Process28prepare_handle_batch_scratch': 512,
        '7Process28reserve_table_storage_charge': 640,
        '7Process31finish_handle_batch_reservation': 768,
        # Keep address-space construction scratch off the subsequent loader
        # mapping/rollback call chain, and off the syscall coordinator itself.
        # Generic handlers also have self-test-only service implementations.
        # Bound the actual syscall path, not those additional instantiations.
        '24sys_process_builder_seal.*15ProcessServices': 1280,
        '14ProcessBuilder11finish_seal': 1024,
        # Keep the charged start transaction off the syscall stack. By-value
        # phase handoffs retained copies below handle-table allocation and
        # IRQ-tail load balancing (4160 bytes before heap ownership).
        '25sys_process_builder_start.*15ProcessServices': 3072,
        # Diagnostic output is bounded scratch, never a heap-allocated snapshot.
        '36sys_memory_inspector_read_statistics.*15ProcessServices': 2048,
        '6loader11load_native': 832,
        '9bootstrap.*5Image5parse$': 768,
        '6loader20create_address_space': 1152,
        # ASID/VMID segment expansion initializes its final heap allocation.
        # A by-value 4 KiB array otherwise survives below later allocation and
        # an asynchronous IRQ-tail scheduler entry, even without expansion.
        '23TranslationEpochSegment7try_new': 256,
        '6loader21prepare_address_space': 768,
        # Guest SMP previously kept two full architectural contexts plus
        # execution temporaries in nested seal/preparation frames (>13 KiB).
        '32sys_pending_virtual_machine_seal.*15ProcessServices': 3072,
        # Preparation retains bounded accounting reservations and scheduler
        # rollback owners (2224 B), not architectural register-bank copies.
        '9VmBuilder17prepare_boot_vcpu': 3072,
        '13VcpuExecution13try_installed': 2048,
        # Dispatch also runs below syscall and exception frames on a 16 KiB
        # stack. Retaining twelve full candidates in local_selection once
        # created a 3216-byte frame and overflowed that stack during std-test.
        '12CpuScheduler15local_selection': 1024,
        '12CpuScheduler12choose_ready': 1024,
        # IRQ-tail balancing must not retain complete thread observations
        # through every migration frame. Selection and mutation keep one lock.
        '^_RNC.*24with_cpu_schedule_stored.*16balance_ready_in0E0': 192,
        '9Scheduler21select_balance_target$': 640,
        '9Scheduler14migrate_thread$': 256,
        '9Scheduler17request_migration$': 256,
        '9Scheduler17move_ready_thread$': 320,
        # Link traversal must read only AVL metadata, not copy full policy and
        # runtime snapshots into every nested mutation frame.
        'ready_index7rebuild.*19ReadyQueueAuthority': 256,
        'ready_index7rebuild.*24LocalReadyQueueAuthority': 256,
        'ready_index7balance.*19ReadyQueueAuthority': 256,
        'ready_index7balance.*24LocalReadyQueueAuthority': 256,
        # A per-CPU heap path replaces the old 336-byte local ancestor array.
        # Both migration and local scheduling must keep that storage off-stack.
        'ready_index.*4Tree.*6remove.*19ReadyQueueAuthority': 256,
        'ready_index.*4Tree.*6remove.*24LocalReadyQueueAuthority': 256,
        # These wrappers may nest under user-buffer admission. Preparation
        # scratch must return before commit waits for a shootdown transaction
        # and schedules on the same kernel stack.
        # Both the logical and machine prepared owners survive phase changes;
        # only their bounded descriptors, not traversal scratch, remain here.
        '18NativeAddressSpace21resolve_private_write': 768,
        '18NativeAddressSpace19populate_file_range': 768,
        # Persistent AVL cursors must use a small ancestor cache, not an array
        # sized for the theoretical maximum tree height. Two old cursors kept
        # more than 2 KiB below usercopy and asynchronous scheduler entry.
        '17KernelPageBackend.*13DomainAccountE26prepare_copy_from_snapshot': 1024,
        '17KernelPageBackend.*13DomainAccountE18prepare_user_write': 1536,
        '18NativeAddressSpace28prepare_private_write_change': 1024,
        # Path lookup already retains traversal/RPC state. Decode metadata
        # into the existing cache; never construct a full name-bearing reply.
        'vfs6remote9transport15decode_response': 512,
        # Keep construction and relative-ancestry temporaries out of ordinary
        # backend lookup waits, which may nest IRQ-tail placement repair.
        'vfs7resolve4walk': 256,
        'vfs7resolve12prepare_walk': 1280,
        'vfs7resolve13restore_start': 1664,
        'vfs7resolve15walk_components': 896,
        'vfs7resolve11finish_walk': 512,
        # RPC publication excludes reply-wait preparation; waiter construction
        # finishes before timer/scheduler entry. Check each live phase below.
        '9Transport8exchange': 384,
        '9Transport13wait_response': 512,
        'wait19prepare_signal_wait': 640,
        # Build the receive registration before output admission. Otherwise
        # its construction temporaries remain below COW/allocator/scheduler.
        '30sys_capability_channel_receive.*15ProcessServices': 4096,
        '25PreparedCapabilityReceive7try_new': 2048,
        # Sender input admission may enter mapping/COW, allocator and scheduler
        # paths. Keep receiver matching/rollback owners off that live chain.
        '31sys_capability_channel_try_send.*15ProcessServices': 768,
        '22PreparedCapabilitySend14copy_from_user': 1024,
        '26send_prepared_capabilities': 4096,
        # File-page faults can reach block waits below loader/usercopy frames.
        # Keep the 1040-byte extent plan in charged per-mount scratch.
        '9Transport12read_scatter': 512,
        'transfer14read_unaligned': 512,
        # Populate file pages before reserving logical/machine mapping scratch.
        '^_RNCNv.*7service8map_file0': 1024,
        '7service20map_loaded_file_view': 768,
        # Admission wait registration must not remain under request commit and
        # completion waits. Bound the slow phase too, not only its coordinator.
        'scatter.*11BlockAccess12read_scatter$': 768,
        '10BlockQueue10wait_space.*11BlockAccess12read_scatter': 1280,
    }
    functions = re.split(r'^\s*[0-9a-f]+ <([^\n]+)>:\s*$', disassembly,
                         flags=re.MULTILINE)
    measured = {}
    for name, budget in budgets.items():
        matches = [functions[i + 1] for i in range(1, len(functions), 2)
                   if re.search(name, functions[i])]
        # Self-test kernels run mechanism tests instead of booting Native init;
        # their unused bootstrap entry is removed by the linker. Still check it
        # if a self-test image retains it, and require it in production images.
        if not matches and self_test and name == '6loader11load_native':
            continue
        if len(matches) != 1:
            raise ValueError(f'expected one emitted {name} function; found {len(matches)}')
        size = entry_frame(matches[0])
        if size == 0 or size > budget:
            raise ValueError(f'{name}: entry frame {size} exceeds contract 1..{budget}')
        measured[name] = size
        print(f'{name}: entry frame {size} bytes (budget {budget})')

    # Moving scratch into a callee must shrink the live chain, not just its
    # caller. Bound each separate phase together with its live coordinator.
    balance = measured['^_RNC.*24with_cpu_schedule_stored.*16balance_ready_in0E0']
    selection = balance + measured['9Scheduler21select_balance_target$']
    if selection > 768:
        raise ValueError(f'balance selection: live frames {selection} exceed 768')
    migration = balance + sum(measured[name] for name in (
        '9Scheduler14migrate_thread$', '9Scheduler17request_migration$',
        '9Scheduler17move_ready_thread$'))
    if migration > 896:
        raise ValueError(f'balance migration: live frames {migration} exceed 896')

    create = measured['26sys_process_builder_create.*15ProcessServices']
    for phase, budget in (('25copy_executable_for_child', 2432),
                          ('14ProcessBuilder15allocate_loaded', 2560)):
        live = create + measured[phase]
        if phase == '14ProcessBuilder15allocate_loaded':
            live += measured['14ProcessBuilder15try_publication']
        if live > budget:
            raise ValueError(f'process creation {phase}: live frames {live} exceed {budget}')

    batch = measured['7Process24reserve_handle_batch_for']
    for phase, budget in (('26prepare_table_storage_plan', 2304),
                          ('28prepare_handle_batch_scratch', 1984),
                          ('31finish_handle_batch_reservation', 2240)):
        live = batch + measured['7Process' + phase]
        if live > budget:
            raise ValueError(f'handle batch {phase}: live frames {live} exceed {budget}')
    table_admission = (batch + measured['7Process26prepare_table_storage_plan']
                       + measured['7Process28reserve_table_storage_charge'])
    if table_admission > 2816:
        raise ValueError(f'handle table admission: live frames {table_admission} exceed 2816')

    seal = measured['24sys_process_builder_seal.*15ProcessServices']
    finish_seal = seal + measured['14ProcessBuilder11finish_seal']
    if finish_seal > 2208:
        raise ValueError(f'process seal publication: live frames {finish_seal} exceed 2208')
    if '6loader11load_native' in measured:
        for phase, budget in (('9bootstrap.*5Image5parse$', 2720),
                              ('6loader20create_address_space', 3072),
                              ('6loader21prepare_address_space', 2752)):
            live = seal + measured['6loader11load_native'] + measured[phase]
            if live > budget:
                raise ValueError(f'process seal {phase}: live frames {live} exceed {budget}')

    scatter_wait = (measured['scatter.*11BlockAccess12read_scatter$']
                    + measured['10BlockQueue10wait_space.*11BlockAccess12read_scatter'])
    if scatter_wait > 1920:
        raise ValueError(f'scatter admission wait: live frames {scatter_wait} exceed 1920')

    unaligned = measured['9Transport12read_scatter'] + measured['transfer14read_unaligned']
    if unaligned > 896:
        raise ValueError(f'unaligned extent read: live frames {unaligned} exceed 896')

    file_mapping = (measured['^_RNCNv.*7service8map_file0']
                    + measured['7service20map_loaded_file_view'])
    if file_mapping > 1536:
        raise ValueError(f'file mapping publication: live frames {file_mapping} exceed 1536')

    for phase, budget in (('12prepare_walk', 1536), ('13restore_start', 1792),
                          ('15walk_components', 1024)):
        live = measured['vfs7resolve4walk'] + measured['vfs7resolve' + phase]
        if live > budget:
            raise ValueError(f'VFS {phase}: live frames {live} exceed {budget}')

    finish = (measured['vfs7resolve4walk'] + measured['vfs7resolve15walk_components']
              + measured['vfs7resolve11finish_walk'])
    if finish > 1408:
        raise ValueError(f'VFS result assembly: live frames {finish} exceed 1408')

    rpc_wait = measured['9Transport8exchange'] + measured['9Transport13wait_response']
    if rpc_wait > 768:
        raise ValueError(f'filesystem RPC wait: live frames {rpc_wait} exceed 768')
    rpc_prepare = rpc_wait + measured['wait19prepare_signal_wait']
    if rpc_prepare > 1344:
        raise ValueError(f'filesystem RPC wait preparation: live frames {rpc_prepare} exceed 1344')

    # Merely outlining output preparation can shrink the caller while growing
    # the live path: both frames remain below COW. Bound their sum as well.
    targets = [functions[i + 1] for i in range(1, len(functions), 2)
               if re.search('23CapabilityReceiveTarget7prepare', functions[i])]
    if len(targets) > 1:
        raise ValueError('ambiguous capability receive target preparation frames')
    target_frame = entry_frame(targets[0]) if targets else 0
    if targets and target_frame == 0:
        raise ValueError('unrecognized capability receive target preparation frame')
    live = measured['30sys_capability_channel_receive.*15ProcessServices'] + target_frame
    if live > 4096:
        raise ValueError(f'capability receive output admission: live frames {live} exceed 4096')

    send_input = (measured['31sys_capability_channel_try_send.*15ProcessServices']
                  + measured['22PreparedCapabilitySend14copy_from_user'])
    if send_input > 1536:
        raise ValueError(f'capability send input admission: live frames {send_input} exceed 1536')

    copy_admission = (
        measured['17KernelPageBackend.*13DomainAccountE26prepare_copy_from_snapshot']
        + measured['17KernelPageBackend.*13DomainAccountE18prepare_user_write'])
    if copy_admission > 2304:
        raise ValueError(f'user write admission: live frames {copy_admission} exceed 2304')


if __name__ == '__main__':
    try:
        check(Path(sys.argv[1]).read_text(),
              self_test=os.environ.get('KERNEL_SELF_TEST_IMAGE') == '1')
    except (ValueError, OSError) as error:
        sys.exit(str(error))
