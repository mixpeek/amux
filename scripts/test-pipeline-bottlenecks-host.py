#!/usr/bin/env python3
"""Host probes retain partial measurements when utility lookup/exec fails."""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('pipeline_bottlenecks', Path(__file__).with_name('pipeline-bottlenecks.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class HostProbeTests(unittest.TestCase):
    def test_standard_system_utility_resolves_without_login_path(self):
        with patch.dict(os.environ, {'PATH': ''}):
            tool = module.host_tool('ps')
            self.assertTrue(tool and Path(tool).is_file(), tool)
            self.assertTrue(Path(tool).is_absolute())
            if Path('/usr/sbin/lsof').is_file():
                self.assertEqual(module.host_tool('lsof'), '/usr/sbin/lsof')

    def measure(self, failure):
        calls = []
        def run(args, **kwargs):
            name = Path(args[0]).name
            calls.append((name, kwargs.get('timeout')))
            if name == 'lsof':
                if failure == 'timeout':
                    raise subprocess.TimeoutExpired(args, kwargs['timeout'])
                return subprocess.CompletedProcess(args, 1, '', 'not measured')
            text = 'total = 8M used = 0M' if name == 'sysctl' else ('123 12.0 2048 test-process\n' if '%cpu=' in ' '.join(args) else '')
            return subprocess.CompletedProcess(args, 0, text, '')
        def lookup(name):
            return None if name == 'lsof' and failure == 'missing' else '/system/' + name
        err = io.StringIO()
        with patch.object(module, 'host_tool', side_effect=lookup), patch.object(module.subprocess, 'run', side_effect=run), patch.object(module, 'sessions_by_conversation', return_value={}), contextlib.redirect_stderr(err):
            result = module.host_measure()
        self.assertEqual(result['top_cpu'][0]['cpu'], 12.0)
        self.assertEqual(result['top_mem'][0]['mb'], 2)
        self.assertEqual(result['top_cpu'][0]['lane'], '?')
        self.assertEqual(result['swap_pct'], 0.0)
        self.assertEqual([e['tool'] for e in result['probe_errors']], ['lsof'])
        self.assertIn('host_probe_partial', err.getvalue())
        self.assertIn('"measured": false', err.getvalue())
        return calls

    def test_absent_attribution_tool_does_not_lose_host_pressure(self):
        self.assertNotIn('lsof', [c[0] for c in self.measure('missing')])

    def test_attribution_timeout_is_bounded_and_logged(self):
        calls = self.measure('timeout')
        self.assertTrue(all(timeout == 5 for tool, timeout in calls if tool == 'lsof'))

    def test_failed_utility_is_unknown_not_zero_or_a_crash(self):
        self.measure('nonzero')

if __name__ == '__main__':
    unittest.main(verbosity=2)
