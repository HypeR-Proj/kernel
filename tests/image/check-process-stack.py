#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Bound AArch64 Process and guest-construction frames in the final optimized ELF disassembly.

This guards the large by-value construction/retirement regression. It is not
a whole-call-graph stack bound; IRQ-tail headroom also needs runtime testing.
"""

import os
import re
import sys
from pathlib import Path


def check(disassembly, *, self_test=False):
    budgets = {
        '15PreparedProcess7try_new': 2048,
        '7Process17finish_retirement': 2048,
        # Keep address-space construction scratch off the subsequent loader
        # mapping/rollback call chain, and off the syscall coordinator itself.
        # Generic handlers also have self-test-only service implementations.
        # Bound the actual syscall path, not those additional instantiations.
        '24sys_process_builder_seal.*23DeferredProcessServices': 2048,
        # Diagnostic output is bounded scratch, never a heap-allocated snapshot.
        '36sys_memory_inspector_read_statistics.*23DeferredProcessServices': 2048,
        '6loader11load_native': 2048,
        '6loader20create_address_space': 6144,
        '6loader21prepare_address_space': 2048,
        # Guest SMP previously kept two full architectural contexts plus
        # execution temporaries in nested seal/preparation frames (>13 KiB).
        '32sys_pending_virtual_machine_seal.*23DeferredProcessServices': 3072,
        # Preparation retains bounded accounting reservations and scheduler
        # rollback owners (2224 B), not architectural register-bank copies.
        '9VmBuilder17prepare_boot_vcpu': 3072,
        '13VcpuExecution13try_installed': 2048,
    }
    functions = re.split(r'^\s*[0-9a-f]+ <([^\n]+)>:\s*$', disassembly,
                         flags=re.MULTILINE)
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
        size = 0
        for line in matches[0].splitlines():
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
        if size == 0 or size > budget:
            raise ValueError(f'{name}: entry frame {size} exceeds contract 1..{budget}')
        print(f'{name}: entry frame {size} bytes (budget {budget})')


if __name__ == '__main__':
    try:
        check(Path(sys.argv[1]).read_text(),
              self_test=os.environ.get('KERNEL_SELF_TEST_IMAGE') == '1')
    except (ValueError, OSError) as error:
        sys.exit(str(error))
