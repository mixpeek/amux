---
description: Use when the user asks to interact with a web page, take a screenshot of a site, click or type in Chrome, scrape content, or debug a web UI. Connects to real Chrome tabs with existing logins.
allowed-tools: Bash, Read
argument-hint: <command> [args...]
context: fork
---

# /chrome-cdp — Chrome Browser Automation

Control the user's **live Chrome browser** via Chrome DevTools Protocol. Connects to real tabs with existing cookies/logins — no fresh browser needed.

## Prerequisites

- Chrome must have remote debugging enabled: `chrome://inspect/#remote-debugging` → toggle the switch
- Node.js 22+ (uses built-in WebSocket)

## Commands

The CLI is at `skills/chrome-cdp/scripts/cdp.mjs` (relative to the amux repo root). Always run from the amux project directory.

### List open tabs

```bash
node skills/chrome-cdp/scripts/cdp.mjs list
```

Returns tab IDs, titles, and URLs. Use the **target ID prefix** (e.g. `6BE827FA`) in all subsequent commands.

### Screenshot

```bash
node skills/chrome-cdp/scripts/cdp.mjs shot <target> [file]
```

Captures the viewport as PNG. Prints DPR and coordinate mapping info.
Default save path: `~/.cache/cdp/screenshot-<target>.png`

**Always read the screenshot file after capturing** to see what's on screen.

### Accessibility tree (preferred for reading page content)

```bash
node skills/chrome-cdp/scripts/cdp.mjs snap <target>
```

Returns a compact semantic tree — much better than raw HTML for understanding page structure.

### Evaluate JavaScript

```bash
node skills/chrome-cdp/scripts/cdp.mjs eval <target> <expression>
```

Runs JS in the page context. Avoid index-based DOM selection across multiple eval calls when the DOM can change between them.

### Navigate

```bash
node skills/chrome-cdp/scripts/cdp.mjs nav <target> <url>
```

Navigates and waits for load completion.

### Click

```bash
node skills/chrome-cdp/scripts/cdp.mjs click <target> <css-selector>
node skills/chrome-cdp/scripts/cdp.mjs clickxy <target> <x> <y>     # CSS pixel coords
```

`click` scrolls the element into view first. `clickxy` takes CSS pixels (screenshot pixels / DPR).

### Type text

```bash
node skills/chrome-cdp/scripts/cdp.mjs type <target> <text>
```

Uses `Input.insertText` — works in cross-origin iframes unlike eval-based approaches. Click/focus the input first.

### Other commands

```bash
node skills/chrome-cdp/scripts/cdp.mjs html <target> [selector]       # full page or element HTML
node skills/chrome-cdp/scripts/cdp.mjs net <target>                    # network resource timing
node skills/chrome-cdp/scripts/cdp.mjs loadall <target> <selector> [ms] # click "load more" until gone
node skills/chrome-cdp/scripts/cdp.mjs evalraw <target> <method> [json] # raw CDP command
node skills/chrome-cdp/scripts/cdp.mjs open [url]                       # open new tab
node skills/chrome-cdp/scripts/cdp.mjs stop [target]                    # stop daemon(s)
```

## Typical workflow

```bash
# 1. List tabs
node skills/chrome-cdp/scripts/cdp.mjs list

# 2. Screenshot a tab
node skills/chrome-cdp/scripts/cdp.mjs shot 6BE827FA /tmp/page.png

# 3. Read the screenshot
# (use Read tool on /tmp/page.png)

# 4. Read page content
node skills/chrome-cdp/scripts/cdp.mjs snap 6BE827FA

# 5. Interact
node skills/chrome-cdp/scripts/cdp.mjs click 6BE827FA "button.submit"
node skills/chrome-cdp/scripts/cdp.mjs type 6BE827FA "Hello world"
```

## Coordinates

Screenshots are at native resolution (CSS pixels × DPR). CDP input events use **CSS pixels**.

```
CSS px = screenshot px / DPR
```

`shot` prints the DPR. Typical Retina (DPR=2): divide screenshot coords by 2.

## Notes

- Chrome shows an "Allow debugging" modal once per tab on first access. A background daemon keeps the session alive so subsequent commands need no further approval.
- Daemons auto-exit after 20 minutes of inactivity.
- Prefer `snap` over `html` for understanding page structure.
- Use `type` (not eval) to enter text — click to focus first, then type.

## Gotchas

- Chrome must have remote debugging enabled (`chrome://inspect/#remote-debugging`) or all commands silently fail.
- First access to a tab triggers an "Allow debugging" modal — you must approve it before commands work.
- `clickxy` uses CSS pixels, not screenshot pixels. Divide screenshot coords by DPR (usually 2 on Retina).
- `eval` results are stringified — objects come back as `[object Object]` unless you `JSON.stringify()`.
- Don't chain multiple `eval` calls that depend on DOM index positions — the DOM can change between calls.
