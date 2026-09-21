#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Regression tests for boundary gate diagnostics and dependency traversal."""

import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('boundary', Path(__file__).with_name('hal-boundary.py'))
boundary = importlib.util.module_from_spec(spec)
spec.loader.exec_module(boundary)


def graph():
    return {
        'packages': [
            {'name': name, 'id': name, 'targets': [{'kind': [kind]}]}
            for name, kind in [('hyper', 'bin'), ('hyper-hal', 'lib'), ('hyper-core', 'lib')]
        ],
        'resolve': {'nodes': [
            {'id': name, 'deps': [{'pkg': dep} for dep in dependencies]}
            for name, dependencies in [('hyper', ['hyper-hal', 'hyper-core']),
                                       ('hyper-hal', ['hyper-core']), ('hyper-core', [])]
        ]},
    }


class GraphTests(unittest.TestCase):
    def test_valid_direction(self):
        boundary.verify_graph(graph())

    def test_transitive_policy_dependency(self):
        metadata = graph()
        metadata['resolve']['nodes'][1]['deps'].append({'pkg': 'bridge'})
        metadata['resolve']['nodes'].append({'id': 'bridge', 'deps': [{'pkg': 'hyper'}]})
        with self.assertRaisesRegex(ValueError, 'forbidden reverse'):
            boundary.verify_graph(metadata)

    def test_core_must_not_depend_on_hal(self):
        metadata = graph()
        metadata['resolve']['nodes'][2]['deps'].append({'pkg': 'hyper-hal'})
        with self.assertRaisesRegex(ValueError, 'forbidden reverse'):
            boundary.verify_graph(metadata)

    def test_policy_library_not_allowed(self):
        metadata = graph()
        metadata['packages'][0]['targets'].append({'kind': ['lib']})
        with self.assertRaisesRegex(ValueError, 'policy must not'):
            boundary.verify_graph(metadata)

    def test_missing_boundary_edge(self):
        metadata = graph()
        metadata['resolve']['nodes'][0]['deps'].clear()
        with self.assertRaisesRegex(ValueError, 'required dependency'):
            boundary.verify_graph(metadata)


class DiagnosticTests(unittest.TestCase):
    def test_expected_private_module_error(self):
        self.assertTrue(boundary.private_arch_error(json.dumps({
            'reason': 'compiler-message',
            'message': {'code': {'code': 'E0603'}, 'message': 'module `arch` is private'},
        })))

    def test_unrelated_failure_cannot_pass(self):
        for code, message in [('E0432', 'unresolved import'),
                              ('E0603', 'module `other` is private'), (None, 'build error')]:
            self.assertFalse(boundary.private_arch_error(json.dumps({
                'reason': 'compiler-message',
                'message': {'code': {'code': code}, 'message': message},
            })))
        self.assertFalse(boundary.private_arch_error('cargo: failed to run rustc'))


if __name__ == '__main__':
    unittest.main()
