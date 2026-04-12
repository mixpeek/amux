---
description: When editing SSE or real-time sync code in amux-server.py
globs: ["amux-server.py"]
---

The SSE system (`_sse_events`, `connectSSE`, `_onClientResume`) is the backbone of live updates. When touching it:
- Server sends a real `{type:"ping"}` data event every 10s — clients use this to detect zombie connections
- Client declares SSE stale after 18s of silence and force-reconnects
- `visibilitychange` / `pageshow` / `focus` / `online` all trigger `_onClientResume` which refetches if data is >4s old
- The polling fallback must fetch BOTH sessions AND board — don't add new data sources to SSE without also adding them to the fallback
- Always test with multiple clients (desktop + phone) to verify all get updates within ~2s
