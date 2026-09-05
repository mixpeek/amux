# Telegram Auto-Reply — Server-Side Relay

**Status**: ✅ Auto-relay works for ALL workers automatically. No per-session configuration needed.

## How It Works (Automatic)

When any session receives a Telegram-routed message (stamped `[from Telegram @user]: ...`) and the session's Claude responds:

1. **Poll Inbound** (every 45s) — `telegram_poll` detects Telegram messages and routes them to linked sessions
2. **Session Responds** — The session's Claude generates a reply in its pane
3. **Relay Automatically** (every 30s) — `telegram_relay` job scans sessions:
   - Finds sessions with active Telegram mappings
   - Detects `[from Telegram @...]` in the pane
   - Extracts NEW text after that message (checkpoint-based)
   - Sends it back to Telegram with HTML formatting
   - Updates checkpoint to avoid duplicate sends

**Zero configuration needed.** Just link a chat (`/link <session>` in Telegram) and send a message.

## Architecture

- **Inbound**: `telegram_poll` (45s interval) → routes Telegram messages to linked sessions
- **Outbound**: `telegram_relay` (30s interval) → auto-sends session replies back to Telegram
- **State**: SQLite checkpoints (last_relayed_line) prevent duplicate sends across server restarts

## For Advanced Users (Explicit Control)

You can also **manually send messages** via the API if you want direct control:

```bash
# From your session: explicitly send a reply
curl -sk -X POST -H 'Content-Type: application/json' \
  -d '{"session":"your-session-name","text":"Your reply"}' \
  $AMUX_URL/api/telegram/send
```

This gives you **explicit control** over when and what gets sent (ethos rule 8).

## Monitoring

Check relay status and last-seen errors:

```bash
# Relay job status + messages_routed counter
curl -sk $AMUX_URL/api/telegram/status

# Mapping details (including last_relayed_at)
curl -sk $AMUX_URL/api/telegram/mappings
```

## Reference Implementation Details

- `crates/amux-server/src/runtime_jobs/telegram_relay.rs` — server-side relay
- `crates/amux-server/migrations/0036_telegram_relay.sql` — relay state tracking (DB)

The old Stop-hook-based relay (`.claude/telegram-relay.py`) is gone — fully
superseded by the server-side job above (works for every session with zero
per-session hook config, unlike the hook it replaced) and removed from the
repo per review (dev artifact, not meant to ship).
