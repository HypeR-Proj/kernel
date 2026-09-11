#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Verify that a non-VHE CPU reaches the explicit pre-MMU rejection loop."""

import json
from pathlib import Path
import re
import os
import select
import struct
import subprocess
import sys
import tempfile
import time


def main():
    qemu, image, elf = sys.argv[1:4]
    nm = sys.argv[4] if len(sys.argv) > 4 else 'llvm-nm'
    symbols = {}
    for line in subprocess.check_output([nm, '--defined-only', elf], text=True).splitlines():
        fields = line.split()
        if len(fields) == 3 and fields[2] in ('_start', 'aarch64_unsupported_vhe'):
            symbols[fields[2]] = int(fields[0], 16)
    with open(image, 'rb') as image_file:
        header = image_file.read(64)
    if len(header) != 64 or header[56:60] != b'ARM\x64':
        raise ValueError('expected an AArch64 Linux Image')
    # QEMU virt RAM starts at 1 GiB; its Linux Image loader honors text_offset.
    load_address = 0x40000000 + struct.unpack_from('<Q', header, 8)[0]
    rejected_pc = load_address + symbols['aarch64_unsupported_vhe'] - symbols['_start']
    with tempfile.TemporaryDirectory(prefix='hyper-vhe-') as temp:
        with open(Path(temp) / 'qemu.log', 'w+b') as log:
            process = subprocess.Popen([
                qemu, '-machine', 'virt,virtualization=on,gic-version=3',
                '-cpu', 'cortex-a72', '-smp', '1', '-m', '256M',
                '-nodefaults', '-display', 'none', '-serial', 'none',
                '-monitor', 'none', '-no-reboot', '-kernel', image,
                '-qmp', 'stdio',
            ], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, bufsize=0)
            try:
                deadline = time.monotonic() + 15
                pending = bytearray()

                def receive():
                    while b'\n' not in pending:
                        remaining = deadline - time.monotonic()
                        if remaining <= 0 or not select.select([process.stdout], [], [], remaining)[0]:
                            raise TimeoutError('QMP response timed out')
                        chunk = os.read(process.stdout.fileno(), 65536)
                        if not chunk:
                            log.seek(0)
                            raise RuntimeError(f'QMP disconnected: {log.read().decode(errors="replace")}')
                        pending.extend(chunk)
                    line, _, rest = pending.partition(b'\n')
                    pending[:] = rest
                    return json.loads(line)

                greeting = receive()
                if 'QMP' not in greeting:
                    raise RuntimeError(f'invalid QMP greeting: {greeting}')

                def execute(command, arguments=None):
                    request = {'execute': command}
                    if arguments is not None:
                        request['arguments'] = arguments
                    process.stdin.write(json.dumps(request).encode() + b'\n')
                    process.stdin.flush()
                    while True:
                        response = receive()
                        if 'error' in response:
                            raise RuntimeError(f'QMP error: {response}')
                        if 'return' in response:
                            return response['return']

                execute('qmp_capabilities')
                registers = ''
                while time.monotonic() < deadline:
                    registers = execute('human-monitor-command',
                                        {'command-line': 'info registers'})
                    match = re.search(r'\bPC=([0-9a-fA-F]+)', registers)
                    # The rejection loop consists of WFE followed by B.
                    if match and int(match[1], 16) in (rejected_pc, rejected_pc + 4):
                        print('FEAT_VHE requirement: non-VHE CPU reached rejection loop')
                        return
                    time.sleep(0.05)
                raise TimeoutError(f'CPU did not reach VHE rejection loop: {registers}')
            finally:
                if process.poll() is None:
                    process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                process.stdin.close()
                process.stdout.close()


if __name__ == '__main__':
    main()
