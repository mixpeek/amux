---
description: Use when you need to test the amux dashboard UI, investigate a visual bug, or verify a frontend change works correctly
allowed-tools: Bash, Read, Grep, Glob, Edit, Write
argument-hint: [run|investigate <description>]
context: fork
---

# /pw-test — Playwright UI Testing

Run the amux Playwright test suite or investigate specific UI issues with a headless browser.

The user's request is: **$ARGUMENTS**

## Setup

- Test suite: `tests/browser-comparison.js` (relative to repo root)
- Auth profile: `~/.amux/playwright-auth/profile`
- Results JSON: `/tmp/pw-test-results.json`
- Screenshots: `/tmp/pw-test-*.png`
- Requires: `npm install` (playwright is a local devDependency)

## Commands

### `run` or empty — Run the full test suite

Run all Playwright UI tests:

```bash
node tests/browser-comparison.js
```

Parse the output and report:
- Total passed/failed count
- List any failures with their error details
- Total runtime
- If all pass, confirm with a brief summary
- If any fail, read the test file to understand what each failing test checks, then suggest fixes

The results JSON is also written to `/tmp/pw-test-results.json` for programmatic access.

### `investigate <description>` — Ad-hoc UI investigation

Use Playwright programmatically to investigate a specific UI issue. Write and run a temporary Node.js script that:

1. Launches a persistent Chromium context with the auth profile:
```javascript
const { chromium } = require('playwright');
const { homedir } = require('os');
const ctx = await chromium.launchPersistentContext(
  `${homedir()}/.amux/playwright-auth/profile`,
  { headless: true, viewport: { width: 1440, height: 900 }, ignoreHTTPSErrors: true }
);
const page = await ctx.newPage();
await page.goto('https://localhost:8822', { waitUntil: 'domcontentloaded', timeout: 10000 });
await page.waitForTimeout(2000);
```

2. Performs whatever DOM inspection, screenshot capture, CSS analysis, or interaction is needed to investigate the described issue.

3. Takes screenshots to `/tmp/pw-investigate-*.png` as evidence.

4. Outputs findings as structured console.log statements.

5. Closes the context when done.

Write the script to `/tmp/pw-investigate.js`, run it with `node /tmp/pw-investigate.js`, and report findings.

**Key patterns for investigation scripts:**
- Use `page.evaluate()` for DOM/CSS inspection
- Use `page.screenshot()` to capture visual state
- Use `page.$eval()` / `page.$$eval()` for element queries
- Use `page.setViewportSize()` to test responsive behavior
- Toggle themes: `page.evaluate(() => { localStorage.setItem('amux_theme', 'light'); document.body.classList.add('light'); })`
- Fetch API data: `page.evaluate(async () => { const r = await fetch('/api/sessions'); return r.json(); })`
- Board data: `page.evaluate(async () => { await fetchBoard(); renderBoard(); })`
- Use `waitUntil: 'domcontentloaded'` (NOT `networkidle` — SSE keeps connections open)

### `add <test description>` — Add a new test

Add a new test case to `tests/browser-comparison.js`:
1. Read the existing test file to understand the pattern
2. Add the new test following the same `record()` pattern
3. Increment test numbering
4. Run the suite to verify the new test passes
5. Report results

## Important Notes

- The server uses a self-signed cert — `ignoreHTTPSErrors: true` is required
- Board data arrives via SSE — use `await fetchBoard()` + `renderBoard()` explicitly
- Default board view is `session` mode; switch to `status` for column headers
- `page.goto` must use `waitUntil: 'domcontentloaded'` (SSE prevents networkidle)
- Screenshots are saved to `/tmp/` — read them with the Read tool to show the user

## Gotchas

- Always use `waitUntil: 'domcontentloaded'` — SSE keeps connections open so `networkidle` never fires.
- Board data is loaded via SSE, not on page load — call `await fetchBoard(); renderBoard();` in `page.evaluate()` before inspecting board DOM.
- The auth profile directory is locked while in use — don't run tests while `playwright-auth capture` is open.
- `ignoreHTTPSErrors: true` is required for the self-signed cert; without it the page won't load.
