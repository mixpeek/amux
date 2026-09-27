#!/usr/bin/env python3
"""A stand-in for `claude -p --output-format stream-json` (AMUX-5263).

Emits the event shapes a real run produces (captured 2026-09-26, see
crates/amux-server/src/api/chat_stream_fixture.jsonl): init, thinking, one
Bash tool call with streamed args and a result, then a markdown answer as
small text deltas, then the result with usage. Paced so a browser can watch
it arrive. FAKE_CLAUDE_DELAY_MS sets the per-delta delay (default 45); a
prompt containing "long answer" streams long enough to press Stop.
"""
import json, os, sys, time

PROMPT = sys.stdin.read()
DELAY = int(os.environ.get("FAKE_CLAUDE_DELAY_MS", "45")) / 1000.0
SID = "0f0e0d0c-0b0a-4908-8706-050403020100"


def out(v):
    sys.stdout.write(json.dumps(v) + "\n")
    sys.stdout.flush()


def se(ev):
    out({"type": "stream_event", "event": ev})


out({"type": "system", "subtype": "init", "session_id": SID, "model": "fake-claude"})
se({"type": "message_start"})
se({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}})
for w in ["Checking ", "the repo ", "layout before ", "answering."]:
    se({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": w}})
    time.sleep(DELAY)
se({"type": "content_block_start", "index": 1,
    "content_block": {"type": "tool_use", "id": "toolu_fake1", "name": "Bash", "input": {}}})
args = '{"command": "ls crates", "description": "List crates"}'
for i in range(0, len(args), 8):
    se({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": args[i:i + 8]}})
    time.sleep(DELAY)
out({"type": "assistant", "message": {"content": [
    {"type": "tool_use", "id": "toolu_fake1", "name": "Bash", "input": json.loads(args)}]}})
time.sleep(DELAY * 8)
out({"type": "user", "message": {"content": [
    {"type": "tool_result", "tool_use_id": "toolu_fake1", "content": "amux-cli\namux-core\namux-dashboard\namux-server", "is_error": False}]}})
se({"type": "message_start"})
answer = """## Workspace crates

The workspace has **four crates**, each with one job:

- `amux-server`: the axum server and runtime jobs
- `amux-dashboard`: the static SPA
- `amux-core` and `amux-cli`: shared types and the CLI

| crate | kind |
|---|---|
| amux-server | binary |
| amux-core | library |

```rust
fn main() {
    println!("streamed");
}
```

That is the whole layout. Ask about any crate for details.
"""
if "long answer" in PROMPT:
    answer += "\n" + "\n\n".join("Paragraph %d keeps streaming so there is time to press Stop." % i for i in range(200))
for i in range(0, len(answer), 6):
    se({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": answer[i:i + 6]}})
    time.sleep(DELAY)
out({"type": "result", "subtype": "success", "is_error": False, "session_id": SID, "total_cost_usd": 0.0123,
     "duration_api_ms": 4000, "num_turns": 2, "result": answer,
     "usage": {"input_tokens": 12, "output_tokens": 180, "cache_read_input_tokens": 900}})
