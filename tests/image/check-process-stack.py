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
        # Keep address-space construction scratch off the subsequent loader
        # mapping/rollback call chain, and off the syscall coordinator itself.
        # Generic handlers also have self-test-only service implementations.
        # Bound the actual syscall path, not those additional instantiations.
        '24sys_process_builder_seal.*15ProcessServices': 2048,
        # Diagnostic output is bounded scratch, never a heap-allocated snapshot.
        '36sys_memory_inspector_read_statistics.*15ProcessServices': 2048,
        '6loader11load_native': 2048,
        '6loader20create_address_space': 6144,
        '6loader21prepare_address_space': 2048,
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
        # Build the receive registration before output admission. Otherwise
        # its construction temporaries remain below COW/allocator/scheduler.
        '30sys_capability_channel_receive.*15ProcessServices': 4096,
        '25PreparedCapabilityReceive7try_new': 2048,
        # Sender input admission may enter mapping/COW, allocator and scheduler
        # paths. Keep receiver matching/rollback owners off that live chain.
        '31sys_capability_channel_try_send.*15ProcessServices': 768,
        '22PreparedCapabilitySend14copy_from_user': 1024,
        '26send_prepared_capabilities': 4096,
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


if __name__ == '__main__':
    try:
        check(Path(sys.argv[1]).read_text(),
              self_test=os.environ.get('KERNEL_SELF_TEST_IMAGE') == '1')
    except (ValueError, OSError) as error:
        sys.exit(str(error))
