<!-- rules: parent=../../CLAUDE.md owner=amux-vision enforces=A4,A7,A8 -->
# amux-dashboard

The single-page dashboard the server serves: `static/` (`index.html`, `app.js`, `app.css`,
`sw.js`, `state/`). The Mac app and the phone app show this same page.

## After an edit

- `node --check crates/amux-dashboard/static/app.js` (the PostToolUse hook runs it on saves).
- Before push: `scripts/spa-lint.sh`, `node scripts/button-audit.mjs --check`, and
  `node --test tests/dashboard-outage-recovery.mjs e2e/*.test.mjs`. `node scripts/build-state.mjs`
  regenerates `state/control-registry.json` and `docs/ux/button-feedback-audit.md`.
- The client version is derived: `app.js` and `sw.js` hold `__AMUX_ASSET_VER__`, and the
  server stamps a content hash of `static/` when serving. Do not bump it.

## Reuse the paradigm that exists (A8)

Before building a view, find the one amux already has for that job and render through it: a
folder is the file view (`_feCrumbsHtml`, `_feBuildRow`, `_feParentRow`, `_feEmptyRow`,
`_feStatusText`), a prompt is `showConfirm` / `showPrompt`, a dialog is
`.modal-overlay > .modal`. If it lacks a hook you need (a root, an open callback, a filter),
add the hook to the shared one. Specimen, 2026-10-16: the MDAI tab grew its own folder table,
breadcrumb and "Folder" line beside the Files tab, and Ethan asked for it to be "the same file
directory / file view as any other file view". Catalogue: `docs/ui-style-guide.md`.

## CSS and the phone

- New UI fits the existing `@media (max-width: 600px)` breakpoints. Touch targets are at least
  44x44 on mobile. `env(safe-area-inset-*)` for anything at a screen edge (iOS PWA notch); the
  viewport uses `viewport-fit=cover`, so fixed overlays extend to the physical edges. Flex
  containers must not overflow a 375px screen.
- **One row of controls above the content and one below, on a phone** (Ethan, 2026-10-21). A
  new control joins that row: it scrolls sideways (`flex-wrap:nowrap; overflow-x:auto`) or goes
  behind a `⋯` menu. Never a second row, never a new bar. `e2e/messages-mobile-one-row.spec.ts`
  is the pattern for pinning it. Screenshot at 375px and count the rows before calling a UI
  change done.
- Use the component contract in `docs/ui-style-guide.md` and Settings → Style guide: `.btn`,
  `.input`, the shared dialog helpers, and theme and spacing tokens (no raw px, `var(--font-ui)`
  with no fallback). Fix a recurring pattern in the shared class. Keep the owner's toolbar
  icons. Primary fills pair `--accent` with `--on-accent`; check focus, disabled, loading and
  invalid states in both themes.

## Controls that render per worker

For worker cards, the send composer, peek overlays, or any control drawn per session:

- Test every state combination: a control driven by two booleans has four states. The
  send-split button is `{send,queue} x {active,idle}` and the bug was in one quadrant.
- Test across workers with different statuses (active, idle, waiting, paused, stopped,
  unattributed); clicking one card's control must not change another's.
- Test after a re-render (`_fetchAndRender()` or an SSE update). A fix that survives the first
  render and breaks on refresh is worse than none.
- Text and class must agree: a button that says "Send" and carries `mode-queue` is this bug.
- The peek overlay and the cards draw from different roots (`#peek-overlay` vs `.card`); test
  each.
- A global mode must look global: every card shows the same label after the toggle. Keep
  per-session delivery detail in tooltips.
- An SSE re-render can overwrite `_syncComposerPending`; after editing the composer, check a
  live dashboard with workers changing status.

## Realtime (SSE)

`_sse_events`, `connectSSE` and `_onClientResume` carry live updates:

- The server sends a `{type:"ping"}` data event every 10s; the client declares the stream stale
  after 18s of silence and reconnects.
- `visibilitychange`, `pageshow`, `focus` and `online` all trigger `_onClientResume`, which
  refetches if data is older than 4s.
- The polling fallback fetches BOTH sessions and board. A new data source on SSE needs the
  fallback too.
- Test with two clients (desktop and phone); both should update within about 2s.
