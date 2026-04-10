---
description: Capture browser auth profiles and sync to cloud — log into any site once, use it everywhere
allowed-tools: Bash
argument-hint: [capture|sync|status]
---

# /playwright-auth — Browser Auth Profile Manager

Manages Playwright browser auth profiles so Claude sessions (local and cloud) can access authenticated sites.

## How It Works

- **capture** — opens a real headed browser; you log into GitHub, Google, Notion, etc.; profile is saved to `~/.amux/playwright-auth/profile/` and automatically synced to cloud
- **sync** — ships the saved profile to the cloud VM via gcloud IAP
- **status** — shows profile age and whether it's been synced

## Instructions

The user's request is: **$ARGUMENTS**

Parse the argument:

### `capture` or empty
Run the capture command and wait for completion:
```bash
amux playwright-auth capture
```
This opens a browser. Inform the user to log into any sites they need, then close the browser. It will automatically sync to cloud when done.

### `sync`
Sync the existing local profile to cloud:
```bash
amux playwright-auth sync
```
Report the output — confirm success or surface any errors.

### `status`
Check the current profile state:
```bash
# Profile location and age
ls -lh ~/.amux/playwright-auth/profile/ 2>/dev/null && \
  echo "Last modified: $(stat -f '%Sm' -t '%Y-%m-%d %H:%M' ~/.amux/playwright-auth/profile 2>/dev/null || stat -c '%y' ~/.amux/playwright-auth/profile 2>/dev/null)" || \
  echo "No profile found. Run: amux playwright-auth capture"
```

Always report clearly:
- Whether a profile exists and how old it is
- Whether the last sync succeeded
- Next steps if something needs attention
