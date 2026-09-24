#!/usr/bin/env python3
"""Hermetic hook producer, privacy, ordering and install regressions."""
import concurrent.futures
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent

def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'hooks' / filename)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result

observer = module('observer', 'native-status.py')
installer = module('installer', 'install-native-status-hooks.py')

class NativeStatus(unittest.TestCase):
    def test_provider_edges(self):
        expected = {'SessionStart':'idle', 'UserPromptSubmit':'active',
            'PreToolUse':'active', 'PermissionRequest':'blocked',
            'PostToolUse':'active', 'PostToolUseFailure':'active', 'Stop':'idle',
            'Interrupt':'idle', 'SessionEnd':'idle', 'StopFailure':'error',
            'PreCompact':'active', 'PostCompact':'active'}
        for event, state in expected.items():
            self.assertEqual(observer.state_for({'hook_event_name':event}), state)
        self.assertEqual(observer.state_for({'hook_event_name':'PreToolUse', 'tool_name':'AskUserQuestion'}),'waiting')
        self.assertEqual(observer.state_for({'hook_event_name':'PreToolUse', 'tool_name':'functions.request_user_input'}),'waiting')
        self.assertIsNone(observer.state_for({'hook_event_name':'Notification','notification_type':'idle_prompt'}))
        self.assertEqual(observer.state_for({'hook_event_name':'Notification','notification_type':'permission_prompt'}),'blocked')
        self.assertIsNone(observer.state_for({'hook_event_name':'SubagentStop'}))

    def test_spool_retains_every_edge_without_server_and_never_prompt_content(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            data={'hook_event_name':'PreToolUse','session_id':'thread','turn_id':'turn',
                  'prompt':'private prompt','tool_input':{'secret':'never retained'}}
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                self.assertTrue(all(pool.map(lambda _: observer.observe(data,root,'worker','abc','codex',100.0),range(32))))
            folder=root/'status-events/worker/abc'
            files=sorted(p for p in folder.glob('*.json') if p.name!='counter.json')
            self.assertEqual(len(files),32)
            values=[json.loads(p.read_text()) for p in files]
            self.assertEqual([v['sequence'] for v in values],list(range(1,33)))
            self.assertNotIn('private prompt',json.dumps(values))
            self.assertNotIn('secret',json.dumps(values))
            self.assertTrue(all(v['event_ts']==100 for v in values))
            self.assertFalse(observer.observe(dict(data,agent_id='child'),root,'worker','abc','codex',101))

    def test_question_notification_preserves_waiting_but_real_permission_blocks(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for i, data in enumerate([
                {'hook_event_name':'PreToolUse','tool_name':'AskUserQuestion'},
                {'hook_event_name':'PermissionRequest','tool_name':'AskUserQuestion'},
                {'hook_event_name':'Notification','notification_type':'permission_prompt'},
                {'hook_event_name':'PostToolUse'},
                {'hook_event_name':'PermissionRequest','tool_name':'Bash'},
            ], 1):
                observer.observe(data, root, 'worker', 'abc', 'claude', float(i))
            values=[json.loads(p.read_text())['state'] for p in sorted((root/'status-events/worker/abc').glob('0*.json'))]
            self.assertEqual(values, ['waiting','waiting','waiting','active','blocked'])

    def test_installer_preserves_other_hooks_idempotently_without_trust_changes(self):
        for provider in ('claude','codex'):
            other={'type':'command','command':'echo other'}
            data={'unrelated':42,'hooks':{'Stop':[{'hooks':[other]}]}}
            first=installer.merge(data,provider,Path('/tmp/a path/native-status.py'))
            saved=json.dumps(first,sort_keys=True)
            second=installer.merge(first,provider,Path('/tmp/a path/native-status.py'))
            self.assertEqual(saved,json.dumps(second,sort_keys=True))
            self.assertEqual(second['unrelated'],42)
            self.assertEqual(second['hooks']['Stop'][0]['hooks'],[other])
            self.assertNotIn('bypass',json.dumps(second))
            self.assertIn('PermissionRequest',second['hooks'])
            self.assertNotIn('SubagentStop',second['hooks'])

if __name__=='__main__': unittest.main()
