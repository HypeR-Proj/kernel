# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""A bounded PTY session for real menuconfig interaction and terminal cleanup."""

import errno
import fcntl
import os
import pty
import select
import signal
import struct
import subprocess
import termios
import time


class Terminal:
    def __init__(self, command, size=(24, 100), cwd=None):
        self.master, self.slave = pty.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack('HHHH', *size, 0, 0))
        self.original = termios.tcgetattr(self.slave)
        self.output = b''
        self.process = subprocess.Popen(command, cwd=cwd, stdin=self.slave,
                                        stdout=self.slave, stderr=self.slave,
                                        env=os.environ | {'TERM': 'xterm'}, start_new_session=True)

    def send(self, text):
        os.write(self.master, text)

    def read(self, timeout):
        if select.select([self.master], [], [], timeout)[0]:
            try:
                self.output += os.read(self.master, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise

    def expect(self, text):
        deadline = time.monotonic() + 15
        while text not in self.output:
            if time.monotonic() >= deadline:
                raise AssertionError(f'missing {text!r}: {self.output[-6000:]!r}')
            self.read(0.05)

    def finish(self):
        deadline = time.monotonic() + 15
        while self.process.poll() is None:
            if time.monotonic() >= deadline:
                raise AssertionError(f'menu did not exit: {self.output[-6000:]!r}')
            self.read(0.05)
        self.read(0.05)
        if self.process.returncode != 0:
            raise AssertionError(self.output[-6000:])
        if not self.restored():
            raise AssertionError('terminal settings were not restored')

    def restored(self):
        actual = termios.tcgetattr(self.slave)
        expected = self.original.copy()
        # BSD sets PENDIN while resuming canonical input. It is a kernel-owned
        # pending-input state, not an unrestored application terminal mode.
        actual[3] &= ~getattr(termios, 'PENDIN', 0)
        expected[3] &= ~getattr(termios, 'PENDIN', 0)
        return actual == expected

    def close(self):
        try:
            os.killpg(self.process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        self.process.wait(timeout=5)
        os.close(self.master)
        os.close(self.slave)
