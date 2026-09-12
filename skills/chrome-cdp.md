---
description: Use when the user asks to interact with a web page, take a screenshot of a site, click or type in Chrome, scrape content, or debug a web UI. Connects to real Chrome tabs with existing logins.
allowed-tools: Bash, Read
argument-hint: <command> [args...]
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

## iOS Simulator Safari (amux Browser target picker)

On a Mac with Xcode, the Browser view now offers the locally detected iOS
runtime/device alongside Desktop Chrome. Open the desired device in Simulator,
select it, enter a URL and press Go. This runs real iOS Safari through Apple's
WebDriver or the configured Appium/XCTest driver; Chrome's phone viewport preset still only resizes Chrome.

For native taps and software-keyboard input, run `scripts/ios-browser-driver.sh setup`
once, then `scripts/ios-browser-driver.sh install-agent` for a supervised local service
(or `scripts/ios-browser-driver.sh run` in the foreground). Set
`AMUX_IOS_WEBDRIVER_PORT=18102` in amux's server.env and restart amux. The helper
binds only loopback; Appium starts XCTest for the explicitly selected simulator.
First XCTest launch has a 180-second deadline; subsequent driver operations have 35-second deadlines. Page evaluation awaits promises and reports rejected scripts as failures.
A missing configured driver produces a visible error, never a fallback to Chrome.

Workers use the same mechanical browser verbs under `/api/browser/ios`:

```bash
curl -sk "$(amux url)/api/browser/ios/targets"
curl -sk "$(amux url)/api/browser/ios/start" -H 'Content-Type: application/json' \
  -d '{"session":"YOUR_WORKER","udid":"UDID_FROM_TARGETS","url":"https://example.com"}'
curl -sk "$(amux url)/api/browser/ios/state?session=YOUR_WORKER"
curl -sk "$(amux url)/api/browser/ios/action" -H 'Content-Type: application/json' \
  -d '{"session":"YOUR_WORKER","action":"click","selector":"button.submit"}'
curl -sk "$(amux url)/api/browser/ios/screenshot?session=YOUR_WORKER"
curl -sk "$(amux url)/api/browser/ios/stop" -H 'Content-Type: application/json' \
  -d '{"session":"YOUR_WORKER"}'
```

`start` also navigates an existing same-worker session. `action` supports
`click` (selector/index/coordinates), `type`, `input`, `key`, `scroll`,
`back`, `eval`, and `extract`. Native mode uses XCTest taps, typing and scroll gestures; optional scroll `x,y` targets a specific region in device points. Responses
and audit rows name `input_method: xcuitest`. Safari-only mode uses WebKit editing
commands, explicitly named `webkit-editor`. Observe the effect after an action.
Native screenshots include browser chrome and the software keyboard; coordinate
clicks use the returned device-point viewport. Safari-only screenshots use web
viewport coordinates. Always use the screenshot response’s coordinate space. `state` supplies the same indexed element list
as Chrome. Screenshot responses include `serve`, a remotely readable PNG route;
read the image after taking it. Pass `X-Amux-Simulator: UDID` on subsequent verbs
to reject an accidental device mismatch. Session is mandatory (or use
`X-Amux-Session`); Safari automation is exclusive and another worker gets 409,
never takeover. Stop releases the owned automation session; it does not shut down
the simulator. Native mode preserves Safari rather than resetting its data. Ownership survives amux server restarts.

`targets` distinguishes unavailable discovery (`measured:false`) from no
installed iOS devices (`measured:true`, `n_considered:0`). Versions come from
CoreSimulator/capabilities, not Safari's frozen user-agent OS token. Chrome
profiles, CDP console/network capture and viewport emulation aren't offered in
Safari. Native mode allows real keyboard and browser-chrome tests. Background
lifecycle and physical-device differences still require explicit coverage; a
WebDriver success response alone is not an end-to-end verdict. Reference: https://webkit.org/blog/9395/webdriver-is-coming-to-safari-in-ios-13/

Run `AMUX_IOS_TEST_URL=https://localhost:18854 node scripts/test-ios-browser.mjs`
against an isolated amux server for real-Simulator browser and interaction-state
regressions. This harness deliberately refuses the production port.

Set `AMUX_IOS_LIFECYCLE=1` for the native main-view and board-creation walkthrough.
The JSON report retains each failed surface; screenshots must be inspected.
