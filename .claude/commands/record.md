---
description: Use when the user wants to record a video of a browser task, create a screen recording, or produce an MP4 demo
allowed-tools: Bash
argument-hint: "<task description>"
context: fork
---

# /record — Browser Screen Recorder

Records an MP4 video of the amux browser agent performing a task. The agent uses Claude to drive a headless Playwright browser, records everything, then removes idle frames (Claude-thinking pauses) via ffmpeg mpdecimate, producing a clean, snappy MP4.

The user's request is: **$ARGUMENTS**

## Instructions

### Step 1 — Start the agent

```bash
curl -sk -X POST -H 'Content-Type: application/json' \
  -d "{\"task\": \"$ARGUMENTS\"}" \
  https://localhost:8822/api/browser/agent
```

Check the response. If `ok` is false, report the error and stop.

### Step 2 — Stream progress until done

Poll every 2 seconds and print each step as it happens:

```bash
while true; do
  STATUS=$(curl -sk https://localhost:8822/api/browser/agent/status)
  STEP=$(echo "$STATUS" | python3 -c "import sys,json; d=json.load(sys.stdin); print(f\"[Step {d['step']}] {d['action']}\")" 2>/dev/null)
  echo "$STEP"
  RUNNING=$(echo "$STATUS" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['running'])" 2>/dev/null)
  [ "$RUNNING" = "False" ] && break
  sleep 2
done
```

Show each step line to the user as it prints.

### Step 3 — Report the result

```bash
curl -sk https://localhost:8822/api/browser/agent/status
```

Parse the final status:
- If `video_ready` is true → tell the user the recording is ready and give them the download URL: **https://localhost:8822/api/browser/video**
  - Also mention: they can click "Download MP4" in the amux Browser tab
- If `error` is set → report the error clearly
- If neither → report "Done (no video produced)"

## Notes

- The agent auto-records — no manual toggle needed
- Idle/thinking frames are automatically removed so the video is snappy
- Max 25 browser steps by default
- Uses the currently active browser profile (for authenticated sessions, switch profiles in the Browser tab first)
- To stop a running recording early: `curl -sk -X POST https://localhost:8822/api/browser/agent/stop`

## Gotchas

- The agent uses the **currently active** browser profile — switch profiles in the Browser tab before recording if you need specific auth.
- `ffmpeg` must be installed for idle-frame removal; without it the raw (unprocessed) video is kept but will have long pauses.
- Max 25 browser steps by default — complex multi-page flows may hit this limit and stop early.
- The video endpoint (`/api/browser/video`) only serves the most recent recording — start a new one and the previous is overwritten.
