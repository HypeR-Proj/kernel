#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Terminal presentation for hyper-kconfig; the Rust tool owns all policy."""

import curses
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import textwrap


class Configuration:
    def __init__(self, backend, source, output, working):
        self.backend = backend
        # Follow an existing symlink when saving instead of replacing the link.
        self.output = output.resolve()
        self.working = working
        shutil.copyfile(source, working)
        self.saved = self.output.read_bytes() if self.output.exists() else None
        self.state = self.update()

    def update(self, name=None, value=None):
        command = [self.backend, 'menuconfig-state', str(self.working)]
        if name is not None:
            command += [name, value]
        result = subprocess.run(command, capture_output=True, text=True, check=False)
        if result.returncode:
            raise ValueError(result.stderr.strip())
        self.state = json.loads(result.stdout)
        return self.state

    def dirty(self):
        return self.saved != self.working.read_bytes()

    def save(self):
        # Keep the existing file intact on an interrupted or failed write. The
        # only publication point is rename, after the Rust backend validated it.
        content = self.working.read_bytes()
        save_bytes(self.output, content)
        self.saved = content


def save_bytes(output, content):
    """Atomically publish a validated configuration, preserving its file mode."""
    descriptor, name = tempfile.mkstemp(prefix='.hyper-config-', dir=output.parent)
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, 'wb') as stream:
            if output.exists():
                os.fchmod(stream.fileno(), output.stat().st_mode & 0o777)
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, output)
    finally:
        temporary.unlink(missing_ok=True)


ENTER = ('\n', '\r', curses.KEY_ENTER, curses.KEY_RIGHT)
BACK = ('q', 'Q', '\x1b', curses.KEY_LEFT)


class Menu:
    def __init__(self, screen, config):
        self.screen = screen
        self.config = config
        self.section = None
        self.search = None
        self.selected = 0
        self.history = []
        self.notice = 'Select an option; changes are saved only with S.'
        if config.state['removed']:
            self.notice = 'Migration pending: removed ' + ', '.join(config.state['removed'])

    def put(self, row, text, attribute=0):
        height, width = self.screen.getmaxyx()
        if 0 <= row < height and width > 1:
            # Configuration strings must never become terminal control bytes.
            text = ''.join(char if char.isprintable() else ' ' for char in text)
            try:
                self.screen.addnstr(row, 0, text, width - 1, attribute)
            except curses.error:
                pass  # A terminal resize can race an otherwise in-range draw.

    def draw(self, title, labels, selected, detail='', footer=None):
        self.screen.erase()
        height, width = self.screen.getmaxyx()
        if height < 12 or width < 56:
            self.put(0, 'Resize terminal to at least 56x12; Q cancels.')
            self.screen.refresh()
            return
        marker = ' [modified]' if self.config.dirty() else ''
        self.put(0, self.config.state['title'] + marker, curses.A_BOLD)
        self.put(1, title)
        self.put(2, '-' * (width - 1))
        count = max(1, height - 8)
        start = max(0, min(selected - count + 1, len(labels) - count))
        for index in range(start, min(len(labels), start + count)):
            self.put(3 + index - start, ('> ' if index == selected else '  ') + labels[index],
                     curses.A_REVERSE if index == selected else 0)
        self.put(height - 5, detail)
        self.put(height - 4, self.notice)
        self.put(height - 2, footer or 'Up/Down: move  Enter: edit  Space: toggle  ?: help  /: search')
        if footer is None:
            self.put(height - 1, 'S: save  Q/Left/Esc: back or exit  Ctrl-C: discard unsaved changes')
        self.screen.refresh()

    def key(self):
        while True:
            key = self.screen.get_wch()
            if key == '\x03':
                raise KeyboardInterrupt
            height, width = self.screen.getmaxyx()
            if height < 12 or width < 56:
                if key in BACK:
                    raise KeyboardInterrupt
                if key != curses.KEY_RESIZE:
                    continue
            if height >= 12 and width >= 56 or key == curses.KEY_RESIZE:
                return key

    def move(self, key, selected, count):
        if not count:
            return 0
        page = max(1, self.screen.getmaxyx()[0] - 8)
        if key in (curses.KEY_DOWN, 'j'):
            return min(count - 1, selected + 1)
        if key in (curses.KEY_UP, 'k'):
            return max(0, selected - 1)
        if key == curses.KEY_NPAGE:
            return min(count - 1, selected + page)
        if key == curses.KEY_PPAGE:
            return max(0, selected - page)
        if key == curses.KEY_HOME:
            return 0
        if key == curses.KEY_END:
            return count - 1
        return selected

    def message(self, title, text):
        position = 0
        while True:
            width = max(20, self.screen.getmaxyx()[1] - 4)
            lines = [line for paragraph in text.splitlines()
                     for line in (textwrap.wrap(paragraph, width) or [''])]
            position = min(position, max(0, len(lines) - 1))
            self.draw(title, lines, position, footer='Up/Down/PageUp/PageDown: scroll  Enter/Esc: close')
            key = self.key()
            if key in ENTER + BACK:
                return
            position = self.move(key, position, len(lines))

    def edit_text(self, title, initial):
        value = initial
        cursor = len(value)
        while True:
            self.draw(title, [], 0, footer='Enter: accept  Esc: cancel  Ctrl-U: clear')
            height, width = self.screen.getmaxyx()
            if height >= 12 and width >= 56:
                start = max(0, cursor - (width - 5))
                self.put(4, '> ' + value[start:])
                self.screen.move(4, 2 + cursor - start)
                curses.curs_set(1)
                self.screen.refresh()
            try:
                key = self.key()
            finally:
                curses.curs_set(0)
            if key in ('\n', '\r', curses.KEY_ENTER):
                return value
            if key == '\x1b':
                return None
            if key == '\x15':
                value, cursor = '', 0
            elif key in (curses.KEY_BACKSPACE, '\x7f', '\b') and cursor:
                value = value[:cursor - 1] + value[cursor:]
                cursor -= 1
            elif key == curses.KEY_DC:
                value = value[:cursor] + value[cursor + 1:]
            elif key == curses.KEY_LEFT:
                cursor = max(0, cursor - 1)
            elif key == curses.KEY_RIGHT:
                cursor = min(len(value), cursor + 1)
            elif key in (curses.KEY_HOME, '\x01'):
                cursor = 0
            elif key in (curses.KEY_END, '\x05'):
                cursor = len(value)
            elif isinstance(key, str) and key.isprintable() and len(value) < 4096:
                value = value[:cursor] + key + value[cursor:]
                cursor += len(key)

    def choice(self, entry):
        options = entry['options']
        selected = next(index for index, option in enumerate(options) if option[0] == entry['value'])
        while True:
            labels = [('(*) ' if name == entry['value'] else '( ) ') + prompt
                      for name, prompt in options]
            self.draw(entry['prompt'], labels, selected, footer='Enter/Space: select  Esc: cancel')
            key = self.key()
            if key in BACK:
                return None
            if key in ENTER or key == ' ':
                return options[selected][0]
            selected = self.move(key, selected, len(options))

    def save(self):
        try:
            self.config.save()
            self.notice = 'Saved ' + str(self.config.output)
            return True
        except (OSError, ValueError) as error:
            self.message('Save failed; original file retained', str(error))
            return False

    def leave(self):
        if not self.config.dirty():
            return True
        while True:
            self.draw('Unsaved changes', ['S: save and exit', 'D: discard changes', 'Esc: keep editing'],
                      0, footer='Press S, D or Esc')
            key = self.key()
            if key in ('s', 'S'):
                return self.save()
            if key in ('d', 'D'):
                return True
            if key in ('q', 'Q', '\x1b'):
                return False

    def rows(self):
        entries = self.config.state['entries']
        if self.search is not None:
            query = self.search.casefold().removeprefix('config_')
            return [entry for entry in entries if query in
                    (entry['name'] + ' ' + entry['prompt']).casefold()]
        prefix = self.section + ' / ' if self.section is not None else ''
        children = []
        for entry in entries:
            if entry['section'].startswith(prefix) and entry['section'] != self.section:
                section = prefix + entry['section'][len(prefix):].split(' / ', 1)[0]
                if section not in children:
                    children.append(section)
        return ([{'kind': 'menu', 'section': child,
                  'prompt': child[len(prefix):] + ' --->'} for child in children]
                + [entry for entry in entries if entry['section'] == self.section])

    def enter(self, section=None, search=None):
        self.history.append((self.section, self.search, self.selected))
        self.section, self.search, self.selected = section, search, 0

    def run(self):
        curses.curs_set(0)
        self.screen.keypad(True)
        # A lone Esc must not impose ncurses' usual one-second delay.
        curses.set_escdelay(25)
        while True:
            rows = self.rows()
            labels = [entry['prompt'] if entry['kind'] == 'menu' else self.label(entry)
                      for entry in rows] or ['No matching settings.']
            self.selected = min(self.selected, max(0, len(labels) - 1))
            row = rows[self.selected] if rows else None
            entry = row if row is not None and row['kind'] != 'menu' else None
            detail = '' if entry is None else self.detail(entry)
            title = f'Search: {self.search}' if self.search is not None else self.section or 'Main menu'
            self.draw(title, labels, self.selected, detail)
            key = self.key()
            self.selected = self.move(key, self.selected, len(labels))
            if key in BACK:
                if self.history:
                    self.section, self.search, self.selected = self.history.pop()
                elif self.leave():
                    return
            elif key in ('s', 'S'):
                self.save()
            elif key == '/':
                query = self.edit_text('Search symbol name or prompt', '')
                if query is not None:
                    self.enter(search=query)
            elif key == '?':
                text = ('Select a category and edit its settings. Save explicitly with S; '
                        'Q at the main menu offers to save or discard changes.')
                if entry is not None:
                    text = self.detail(entry) + '\n\n' + (entry['help'].strip() or 'No additional help.')
                self.message('Help', text)
            elif row is not None and row['kind'] == 'menu' and key in ENTER:
                self.enter(section=row['section'])
            elif entry is not None and (key in ENTER or key in (' ', 'y', 'n', 'Y', 'N')):
                self.edit(entry, key)

    def edit(self, entry, key):
        value = None
        if entry['kind'] == 'choice' and key not in ('y', 'n', 'Y', 'N'):
            value = self.choice(entry)
        elif entry['kind'] == 'bool':
            value = key.lower() if key in ('y', 'n', 'Y', 'N') else ('n' if entry['value'] == 'y' else 'y')
        elif key in ENTER:
            value = self.edit_text(entry['prompt'], entry['value'])
        if value is not None:
            try:
                self.config.update(entry['name'], value)
                self.notice = 'Changed ' + entry['name'] + '; not yet saved'
            except ValueError as error:
                self.message('Invalid value; setting unchanged', str(error))

    @staticmethod
    def label(entry):
        if entry['kind'] == 'bool':
            return ('[*] ' if entry['value'] == 'y' else '[ ] ') + entry['prompt']
        if entry['kind'] == 'choice':
            selected = next(prompt for name, prompt in entry['options'] if name == entry['value'])
            return entry['prompt'] + ' (' + selected + ') --->'
        return '(' + entry['value'] + ') ' + entry['prompt']

    @staticmethod
    def detail(entry):
        label = ('Choice ' if entry['kind'] == 'choice' else 'CONFIG_') + entry['name']
        if entry['range']:
            label += '  range: ' + '..'.join(map(str, entry['range']))
        return label


def main():
    if len(sys.argv) != 5:
        raise ValueError('usage: menuconfig.py BACKEND INPUT OUTPUT DEFCONFIG')
    if not sys.stdin.isatty() or not sys.stdout.isatty() or os.environ.get('TERM') in (None, '', 'dumb'):
        raise ValueError('menuconfig needs an interactive terminal with TERM set; use make config for line input')
    backend, source, output, defaults = sys.argv[1:]
    source = Path(source)
    if not source.exists():
        source = Path(defaults)
    with tempfile.TemporaryDirectory(prefix='hyper-menuconfig-') as directory:
        config = Configuration(backend, source, Path(output), Path(directory) / 'working.config')
        curses.wrapper(lambda screen: Menu(screen, config).run())


if __name__ == '__main__':
    try:
        main()
    except KeyboardInterrupt:
        print('menuconfig: unsaved changes discarded', file=sys.stderr)
        sys.exit(130)
    except (OSError, ValueError, curses.error) as error:
        print(f'menuconfig: {error}', file=sys.stderr)
        sys.exit(1)
