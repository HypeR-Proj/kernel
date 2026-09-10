#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Bound AArch64 Process entry frames in the final optimized ELF disassembly.

This guards the large by-value construction/retirement regression. It is not
a whole-call-graph stack bound; IRQ-tail headroom also needs runtime testing.
"""

import re
import sys
from pathlib import Path


def check(disassembly):
    budgets = {
        '15PreparedProcess7try_new': 2048,
        '7Process17finish_retirement': 2048,
    }
    functions = re.split(r'^\s*[0-9a-f]+ <([^\n]+)>:\s*$', disassembly,
                         flags=re.MULTILINE)
    for name, budget in budgets.items():
        matches = [functions[i + 1] for i in range(1, len(functions), 2)
                   if name in functions[i]]
        if len(matches) != 1:
            raise ValueError(f'expected one emitted {name} function')
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
        check(Path(sys.argv[1]).read_text())
    except (ValueError, OSError) as error:
        sys.exit(str(error))
