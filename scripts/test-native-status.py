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

    def test_only_user_prompt_submit_carries_the_prompt_bounded(self):
        # F8(c): the server records a prompt typed into the pane, so the submit
        # edge carries its text. No other edge does.
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            observer.observe({'hook_event_name':'UserPromptSubmit','prompt':'x'*25000},root,'worker','abc','claude',1.0)
            observer.observe({'hook_event_name':'PostToolUse','prompt':'private prompt'},root,'worker','abc','claude',2.0)
            values=[json.loads(p.read_text()) for p in sorted((root/'status-events/worker/abc').glob('0*.json'))]
            self.assertEqual(len(values[0]['prompt']),20000)
            self.assertNotIn('prompt',values[1])

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

    def test_subagent_prompt_blocks_the_lane_and_only_its_own_edge_clears_it(self):
        # tubescience-parity 2026-09-30: a subagent's permission prompt read as
        # WORKING because every subagent edge was dropped.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            seen = [observer.observe(data, root, 'worker', 'abc', 'claude', float(i)) for i, data in enumerate([
                {'hook_event_name':'PreToolUse'},
                {'hook_event_name':'PreToolUse','agent_id':'a1'},
                {'hook_event_name':'PermissionRequest','tool_name':'Bash','agent_id':'a1'},
                {'hook_event_name':'PreToolUse','agent_id':'a2'},
                {'hook_event_name':'PostToolUse','agent_id':'a1'},
                {'hook_event_name':'Stop','agent_id':'a1'},
            ], 1)]
            self.assertEqual(seen, [True, False, True, False, True, False])
            values=[json.loads(p.read_text()) for p in sorted((root/'status-events/worker/abc').glob('0*.json'))]
            self.assertEqual([v['state'] for v in values], ['active','blocked','active'])
            self.assertEqual([v.get('subagent', False) for v in values], [False, True, True])

    def test_stop_failure_carries_its_reason_or_says_it_had_none(self):
        # gs12 review: 37 StopFailure edges carried no reason field.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            observer.observe({'hook_event_name':'StopFailure','error':'rate_limit','x'*3:1}, root, 'worker', 'abc', 'claude', 1.0)
            observer.observe({'hook_event_name':'StopFailure','session_id':'s','other':'never copied'}, root, 'worker', 'abc', 'claude', 2.0)
            observer.observe({'hook_event_name':'StopFailure','error':{'type':'server_error'}}, root, 'worker', 'abc', 'claude', 3.0)
            values=[json.loads(p.read_text()) for p in sorted((root/'status-events/worker/abc').glob('0*.json'))]
            self.assertEqual((values[0]['failure_field'], values[0]['failure_reason']), ('error','rate_limit'))
            self.assertTrue(values[1]['failure_reason_missing'])
            self.assertIsNone(values[1]['failure_reason'])
            self.assertEqual(values[1]['failure_payload_keys'], ['hook_event_name','other','session_id'])
            self.assertNotIn('never copied', json.dumps(values[1]))
            self.assertEqual(values[2]['failure_reason'], 'server_error')

    def test_lock_wait_is_bounded_and_every_overrun_is_counted(self):
        import fcntl
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            folder = root/'status-events/worker/abc'
            folder.mkdir(parents=True)
            saved = observer.LOCK_BUDGET_S
            observer.LOCK_BUDGET_S = 0.05
            try:
                with (folder/'write.lock').open('a') as held:
                    fcntl.flock(held, fcntl.LOCK_EX)
                    self.assertFalse(observer.observe({'hook_event_name':'PreToolUse'}, root, 'worker', 'abc', 'claude', 1.0))
            finally:
                observer.LOCK_BUDGET_S = saved
            errors=[json.loads(l) for l in (root/'status-observer-errors.jsonl').read_text().splitlines()]
            self.assertEqual([e['error_type'] for e in errors], ['lock_budget_exceeded'])
            self.assertEqual(errors[0]['event'], 'PreToolUse')
            self.assertTrue(observer.observe({'hook_event_name':'PreToolUse'}, root, 'worker', 'abc', 'claude', 2.0))

    def test_a_subagent_active_edge_with_nothing_to_clear_never_takes_the_lock(self):
        import fcntl
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            observer.observe({'hook_event_name':'PreToolUse'}, root, 'worker', 'abc', 'claude', 1.0)
            folder = root/'status-events/worker/abc'
            saved = observer.LOCK_BUDGET_S
            observer.LOCK_BUDGET_S = 0.05
            try:
                with (folder/'write.lock').open('a') as held:
                    fcntl.flock(held, fcntl.LOCK_EX)
                    self.assertFalse(observer.observe({'hook_event_name':'PreToolUse','agent_id':'a1'}, root, 'worker', 'abc', 'claude', 2.0))
            finally:
                observer.LOCK_BUDGET_S = saved
            self.assertFalse((root/'status-observer-errors.jsonl').exists(), 'the fast path must not wait for the lock')

    def test_a_held_deliver_lock_is_seen_so_no_second_deliverer_spawns(self):
        import fcntl
        with tempfile.TemporaryDirectory() as tmp:
            folder = Path(tmp)
            self.assertFalse(observer.deliverer_running(folder))
            with (folder/'deliver.lock').open('a') as held:
                fcntl.flock(held, fcntl.LOCK_EX)
                self.assertTrue(observer.deliverer_running(folder))
            self.assertFalse(observer.deliverer_running(folder))

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
