#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Reject FP/SIMD instructions outside the kernel's three owned-state leaves."""

import re
import subprocess
import sys

LEAVES = ('fp_reset', 'fp_restore', 'fp_save_and_clear')
SYMBOL = re.compile(r'^[0-9a-f]+ <([^>]+)>:$')
INSTRUCTION = re.compile(r'^\s*[0-9a-f]+:\s+(\S+)\s*(.*)$')
# General-register-only code must not name scalar, SIMD, SVE, or predicate
# registers, nor FP control/status registers. Decode the final linked image,
# including compiler builtins; inspecting only Rust source is insufficient.
AARCH64_FP_OPERAND = re.compile(r'\b(?:[bhsdqvz](?:[12]?[0-9]|3[01])|p(?:[0-9]|1[0-5])|fpcr|fpsr|fpmr|svcr)\b')

RISCV_FP_OPERAND = re.compile(r'\b(?:f(?:[12]?[0-9]|3[01])|f[ast](?:[0-9]|1[01])|v(?:[12]?[0-9]|3[01])|fcsr|frm|fflags|vl|vtype|vstart|vxsat|vxrm)\b')
RISCV_FP_CONTROL = {'frcsr', 'fscsr', 'frrm', 'fsrm', 'fsrmi', 'frflags', 'fsflags', 'fsflagsi'}


def main():
    if len(sys.argv) != 4:
        raise SystemExit('usage: verify-fp-ownership.py ARCH LLVM_OBJDUMP ELF')
    arch = sys.argv[1]
    if arch not in ('aarch64', 'riscv64'):
        raise SystemExit(f'unsupported FP ownership architecture: {arch}')
    allowed = {f'{arch}_{leaf}' for leaf in LEAVES}
    operand = AARCH64_FP_OPERAND if arch == 'aarch64' else RISCV_FP_OPERAND
    text = subprocess.check_output(
        [sys.argv[2], '--disassemble', '--no-show-raw-insn', sys.argv[3]], text=True)
    symbol = None
    seen = set()
    bad = []
    for line in text.splitlines():
        match = SYMBOL.match(line)
        if match:
            symbol = match[1]
            continue
        # LLVM retains raw bytes for mapping-symbol data even with
        # --no-show-raw-insn; these are literal pools, not instructions.
        if '\t.word\t' in line or '\t.xword\t' in line or '\t.byte\t' in line:
            continue
        match = INSTRUCTION.match(line)
        if not match or not (operand.search(match[2]) or (arch == 'riscv64' and (match[1] in RISCV_FP_CONTROL or match[1].startswith('v')))):
            continue
        if symbol in allowed:
            seen.add(symbol)
        else:
            bad.append(f'{symbol}: {line.strip()}')
    if bad:
        raise SystemExit('unowned kernel FP/SIMD instructions:\n' + '\n'.join(bad[:30]))
    if seen != allowed:
        raise SystemExit(f'missing audited FP-state leaves: {sorted(allowed - seen)}')
    print(f'{arch} kernel FP/SIMD ownership audit passed')


if __name__ == '__main__':
    main()
