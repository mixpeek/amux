#!/usr/bin/env python3
"""Exercise shipped CLI retries with a fake transport, never a worker pane."""
import json
import os
from pathlib import Path
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
source = (root / 'amux').read_text()
start = source.index('_send_via_api() {')
end = source.index('\ncmd_stop()', start)
function = source[start:end]
with tempfile.TemporaryDirectory(prefix='amux-send-retry-') as temp:
    folder = Path(temp)
    harness = folder / 'check.sh'
    harness.write_text('''set -u
RED= YELLOW= RESET= GREEN=
CC_HOME="$TEST_DIR"
_warn_second_person_resend() { :; }
_flush_unstamped_ledger() { :; }
tmux_name() { printf 'amux-%s' "$1"; }
sleep() { :; }
session_backend() { echo tmux; }
tmux_has_session_exact() { return 0; }
tmux() { echo CALLED >> "$TEST_DIR/tmux"; return 0; }
_record_unstamped_send() { echo RECORDED >> "$TEST_DIR/tmux"; }
_curl() {
  local previous= argument body=
  for argument in "$@"; do
    [[ "$previous" == -d ]] && body="$argument"
    previous="$argument"
  done
  printf '%s\\n' "$body" >> "$TEST_DIR/requests"
  local count; count=$(wc -l < "$TEST_DIR/requests")
  if [[ "$MODE" == refused ]]; then echo '{"ok":false,"error":"explicit refusal"}'; return; fi
  if [[ "$MODE" == recovered && "$count" -eq 3 ]]; then echo '{"ok":true,"deduped":true,"message":"duplicate retry ignored"}'; return; fi
  return 28
}
''' + function + '\n_send_via_api target "a multiline\nmessage"\n')
    for mode, expected, attempts in [('outage', 1, 3), ('recovered', 0, 3), ('refused', 1, 1)]:
        for name in ['requests', 'tmux', 'logs/send-failures.jsonl']:
            (folder / name).unlink(missing_ok=True)
        result = subprocess.run(['bash', str(harness)], env={**os.environ, 'MODE':mode, 'TEST_DIR':temp}, capture_output=True, text=True)
        assert result.returncode == expected, (mode, result.stdout, result.stderr)
        requests = [json.loads(line) for line in (folder / 'requests').read_text().splitlines()]
        assert len(requests) == attempts, requests
        ids = {r.get('msg_id') for r in requests}
        assert len(ids) == 1 and next(iter(ids)), requests
        assert all(r['text'] == 'a multiline\nmessage' for r in requests)
        assert not (folder / 'tmux').exists(), 'Unacknowledged sends must not touch a terminal'
        if mode == 'outage':
            row = json.loads((folder / 'logs/send-failures.jsonl').read_text())
            assert row['verdict'] == 'send_delivery_unknown' and row['raw_fallback'] is False
        print(f'PASS {mode}: {attempts} request(s), stable identity, no terminal injection')
