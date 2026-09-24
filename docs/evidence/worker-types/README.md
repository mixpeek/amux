# Worker types: click-only UI evidence (2026-09-24)

`e2e/worker-types-click.cjs` against an isolated server built from
`feature/worker-type`, headless Chromium at 1280x860 plus an iPhone 13 profile.
Every action was a click, tap or keystroke.

Runs 5, 6 and 7 back to back: 31/31, 31/31, 31/31. `results-run7.json` has
each check with the value it measured.

| File | Shows |
|---|---|
| 01-create-chat.png | New worker dialog with Type = Chat; unsupported providers disabled, branch/worktree hidden |
| 03-chat-first-reply.png | The first prompt from the dialog, answered in the Chat view |
| 04-streaming-midway.png | A reply captured mid-stream ("responding…") |
| 09-switched-to-chat.png | A coding worker switched to Chat from Configurations |
| 09b-after-reload.png | Page reloaded with the chat open: window and history restored |
| 12-phone-chat.png | Phone: queued replies in order, card-composer reply, tap-sent message |
