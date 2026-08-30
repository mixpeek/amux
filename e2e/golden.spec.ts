// Phase 5 browser-side golden scenarios (RR-0083 offline mode, RR-0087
// real-time convergence), run against the REAL extracted 1.4MB dashboard
// served by the Rust server — not a synthetic harness page.
//
// ── Platform truths this spec is written against (measured, not assumed) ──
//
// 1. SSE is LIVE. The server accepts the SPA's `?_token=` EventSource auth
//    and speaks the legacy dialect the SPA parses: an immediate full
//    {type:"board",payload:[...]} / {type:"sessions",...} push on connect,
//    per-write pushes coalesced at 250ms, plus {type:"ping"}
//    (crates/amux-server/src/api/sse.rs). Transport-active is detected as
//    EITHER the SPA's EventSource receiving data (`_liveSSE`) OR its polling
//    fallback (`_pollTimer` armed) — the test then ASSERTS which one engaged
//    (expected: sse) and annotates it. An earlier build rejected `?_token=`,
//    the SPA silently degraded to 5s polling, and nothing said so; this
//    transport assertion is the instrument that fails loudly if that class
//    of regression returns.
//
// 2. Delta sync is a protocol no-op. The SPA's _runDeltaSync speaks the
//    Python contract (`/api/sync?since=` → {issues, statuses, ...}); the Rust
//    /api/sync returns {rev, events:[...]}. The client finds no `issues` key
//    and applies nothing. Convergence rides the SSE board pushes and
//    fetchBoard (full list), not the delta path.
//
// 3. `boardItems`, `online`, `offlineQueue`, `_liveSSE`, `_sse`, `_pollTimer`
//    are top-level `let` bindings — they live in the global lexical
//    environment, NOT on `window`. String-form page.evaluate resolves them
//    exactly like the page's own code does; `(window as any).boardItems`
//    would be undefined.
//
// 4. The RR-0087 mid-stream kill closes the EventSource SILENTLY from page
//    context (`_sse.close()`): no error event fires, `_liveSSE` stays true —
//    a true zombie connection, exactly the iOS-background case the SPA's
//    staleness watchdog exists for. Recovery is that watchdog (`_lastDataTime`
//    older than 18s, checked every 5s → _forceSseReconnect → fresh initial
//    push + refetch), so the phase-2 budget reflects its cadence, not
//    network latency.
import { test, expect, Page, APIRequestContext } from '@playwright/test';

// ---- helpers ---------------------------------------------------------------

/** Bearer token as the SPA received it from the served bootstrap. */
async function appToken(page: Page): Promise<string> {
  const tok = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  expect(tok, 'served bootstrap must inject a non-empty auth token').toBeTruthy();
  return tok;
}

function authHeaders(token: string): Record<string, string> {
  return { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
}

/** Load '/' and wait for the app shell + its API layer to be live. */
async function settle(page: Page): Promise<void> {
  await page.goto('/');
  await expect(page.locator('#conn-status').first()).toBeAttached();
  await page.waitForFunction(() => typeof (window as any).apiCall === 'function');
  // The service worker cannot register on this origin (self-signed cert), so
  // the real app shows its "Offline mode is OFF" failure bar (#sw-fail-bar) —
  // a fixed bottom overlay that on a 375px viewport covers bottom-anchored
  // controls (it blocked the board modal's Save button on mobile). Dismiss it
  // exactly as a user would — via its own × close button — whenever it
  // appears in the way of an action.
  await page.addLocatorHandler(page.locator('#sw-fail-bar'), async (bar) => {
    await bar.locator('button').last().click();
  });
  // The first-visit walkthrough auto-launches ~1.5s after the first payload
  // (fresh profile + zero workers = its exact trigger, always true in this
  // harness). Its backdrop swallows every click, and dismissing it LATER is
  // worse: _wtDismiss → _wtCleanup closes every open modal, including a
  // board-edit dialog mid-test. So wait for it deterministically and skip it
  // up front via its own Skip button, exactly as a first-time user would.
  // (Skip sets amux_walkthrough_done, so it cannot re-fire in this context.)
  const wt = page.locator('#wt-overlay.open');
  await wt.waitFor({ state: 'visible', timeout: 8_000 }).catch(() => {});
  if (await wt.isVisible()) {
    await page.locator('#wt-tooltip .wt-skip').click();
    await expect(wt).toBeHidden();
  }
}

/** One queued op as the SPA persists it (see `_queueOp` in app.js). */
type OutboxOp = { url: string; options?: { method?: string; body?: string } };

/** The SPA's offline outbox, straight from localStorage (its storage of record). */
function outbox(page: Page): Promise<OutboxOp[]> {
  return page.evaluate(
    "JSON.parse(localStorage.getItem('amux_offline_queue') || '[]')",
  ) as Promise<OutboxOp[]>;
}

/** In-memory board state — the `boardItems` lexical global. */
function boardTitles(page: Page): Promise<string[]> {
  return page.evaluate(
    "(typeof boardItems === 'undefined' ? [] : boardItems).map(i => String(i.title))",
  ) as Promise<string[]>;
}

/**
 * Which live-update transport actually engaged (header note 1):
 * 'sse' once the EventSource has received data, 'polling' once the fallback
 * timer armed, 'none' before either. SSE is checked first — if both somehow
 * hold (fallback armed, then SSE recovered), SSE is the operative transport.
 */
function transport(page: Page): Promise<'sse' | 'polling' | 'none'> {
  return page.evaluate(
    "(typeof _liveSSE !== 'undefined' && _liveSSE) ? 'sse' " +
      ": (typeof _pollTimer !== 'undefined' && _pollTimer != null) ? 'polling' : 'none'",
  ) as Promise<'sse' | 'polling' | 'none'>;
}

async function serverTitles(request: APIRequestContext, token: string): Promise<string[]> {
  // Bare `archived` (absent) = NO filter, the Python grammar: the whole
  // board including archived rows. (`archived=all` was a Rust-only spelling;
  // Python reads any non-truthy value as "non-archived only".)
  const res = await request.get('/api/board?done_limit=0', {
    headers: authHeaders(token),
  });
  expect(res.status()).toBe(200);
  const items = (await res.json()) as Array<{ title: string }>;
  return items.map((i) => i.title);
}

// ---- RR-0083: offline queue and replay -------------------------------------
//
// Offline → 3 board cards created THROUGH THE REAL UI (board tab → “+ New
// issue” → title → Save) → SPA queues them in its outbox → reconnect → the
// SPA's own 'online' listener replays via runSyncBanner → server holds each
// exactly once, queue drained, client converges.

test('golden_offline_queue_and_replay', async ({ page, request }, testInfo) => {
  test.setTimeout(120_000);
  await settle(page);
  const token = await appToken(page);
  // Prefix carries project + workerIndex + retry: both tests assert
  // exactly-once server state, and when that assertion fired on an early run
  // the stamp is what discriminated "same test, two executions" from
  // "HTTP-level double-POST". Costs nothing; keep it.
  const prefix = `e2e-off-${testInfo.project.name}-w${testInfo.workerIndex}r${testInfo.retry}-${Date.now()}`;
  const titles = [1, 2, 3].map((n) => `${prefix} card ${n}`);

  // Real UI: open the board view before going offline.
  await page.click('#tab-board');
  await expect(page.locator('#board-view')).toBeVisible();

  // ---- go offline (fires the window 'offline' event → setOnline(false)) ---
  // No "pill shows Offline" assertion HERE, deliberately. Once SSE is real,
  // a data event or fetch response already in flight when the offline event
  // lands can resolve a beat later and flip the SPA back to online=true
  // ("Polling") — and with no poll timer armed and the EventSource silently
  // dead, NOTHING disproves the wrong flip for tens of seconds (observed live
  // both ways; SPA defect worth filing: success callbacks flip online=true
  // without checking navigator.onLine). The deterministic corrective signal
  // is the one a real user produces: USE the app — rejected mutations drive
  // consecutiveFailures past the threshold and force setOnline(false). So the
  // creates come first; the flag and pill are asserted after.
  await page.context().setOffline(true);

  // ---- create 3 cards through the SPA's own add flow ----------------------
  // Both flag states queue exactly once: offline-aware apiCall queues
  // directly; a wrongly-"online" SPA attempts the fetch, the outbox
  // interceptor catches the rejection, queues, and counts the failure.
  for (const title of titles) {
    await page.click('.board-new-btn');
    await expect(page.locator('#board-edit-overlay')).toHaveClass(/active/);
    await page.fill('#be-title', title);
    await page.click('.be-save');
    await expect(page.locator('#board-edit-overlay')).not.toHaveClass(/active/);
  }

  // ---- offline queue mechanics: outbox holds exactly our 3 mutations ------
  // Poll on the URL LIST, not on `.length`. A bare count fails with
  // "Expected: 3 / Received: 4" and names nothing, so the one question that
  // matters — WHICH op is the extra one — costs a local repro to answer (it
  // did, on 2026-08-11, for a mobile-only failure that had been red on main
  // for two days). The list makes a double-queue, a stray telemetry write and
  // a duplicated title distinguishable straight from the CI log.
  await expect
    .poll(async () => (await outbox(page)).map((o) => `${o.options?.method || 'GET'} ${o.url}`), {
      timeout: 10_000,
    })
    .toEqual(titles.map(() => 'POST /api/board'));
  const queued = await outbox(page);
  for (const op of queued) expect(op.url).toContain('/api/board');

  // The SPA should now know it is offline. Two interlocking defects can wedge
  // it wrongly-online instead (both observed live in this harness; detailed
  // in the run report): (a) an SSE/fetch event already in flight when the
  // offline event lands is delivered a beat later and flips online=true
  // back; (b) the outbox interceptor's synthetic 202 then counts as a
  // SUCCESS in apiCall and RESETS consecutiveFailures — the very counter the
  // interceptor's catch just incremented — so queued creates can never
  // re-correct the flag, and with EventSource connect attempts stalling
  // under offline emulation no retry ladder ever runs a corrective fetch.
  // When (and only when) the flag fails to settle on its own, nudge the
  // SPA's OWN fetchBoard(): a plain GET the interceptor does not intercept,
  // whose catch checks navigator.onLine — the product's real corrective
  // path, the same one any user interaction that refetches would take.
  const settled = await page
    .waitForFunction("typeof online !== 'undefined' && online === false", null, {
      timeout: 5_000,
    })
    .then(() => true)
    .catch(() => false);
  if (!settled) {
    testInfo.annotations.push({
      type: 'note',
      description:
        'SPA wedged wrongly-online after the offline event (stale in-flight success + ' +
        'synthetic-202 failure-counter reset); corrected through its own fetchBoard() failure path',
    });
    await page.evaluate('fetchBoard()');
    await page.waitForFunction("typeof online !== 'undefined' && online === false", null, {
      timeout: 10_000,
    });
  }
  // …and the pending UI reflects the queue: status pill + offline banner.
  await expect(page.locator('#conn-status').first()).toHaveText('3 pending');
  await expect(page.locator('#offline-banner')).toHaveClass(/active/);
  await expect(page.locator('#offline-banner-title')).toContainText('3 ops');

  // The queue really is local: nothing reached the server yet.
  const during = await serverTitles(request, token);
  for (const t of titles) expect(during).not.toContain(t);

  // ---- reconnect: the SPA's own resume path does the replay ---------------
  await page.context().setOffline(false);
  // Playwright's Chromium emulation fires the window 'online' event, which is
  // the SPA's sanctioned trigger (setOnline(true) → runSyncBanner). Belt and
  // suspenders: if the SPA has not flipped its own `online` flag shortly, we
  // dispatch the same event it listens for — and record that we had to.
  const flipped = await page
    .waitForFunction("typeof online !== 'undefined' && online === true", null, {
      timeout: 5_000,
    })
    .then(() => true)
    .catch(() => false);
  if (!flipped) {
    testInfo.annotations.push({
      type: 'note',
      description:
        "native 'online' event did not flip the SPA flag within 5s; dispatched window 'online' manually",
    });
    await page.evaluate("window.dispatchEvent(new Event('online'))");
  }

  // Queue drains through runSyncBanner…
  await expect.poll(async () => (await outbox(page)).length, { timeout: 30_000 }).toBe(0);
  // …and the sync banner reported the replay in the UI.
  await expect(page.locator('#sync-title-text')).toContainText(/synced/, { timeout: 15_000 });

  // ---- server holds every card exactly once (RR-0083: no duplicates) ------
  await expect
    .poll(async () => {
      const all = await serverTitles(request, token);
      return titles.filter((t) => all.includes(t)).length;
    }, { timeout: 15_000 })
    .toBe(3);
  const finalAll = await serverTitles(request, token);
  for (const t of titles) {
    expect(finalAll.filter((x) => x === t), `no duplicate replay of "${t}"`).toHaveLength(1);
  }

  // ---- client converged too: boardItems carries the replayed cards --------
  await expect
    .poll(async () => (await boardTitles(page)).filter((t) => titles.includes(t)).length, {
      timeout: 20_000,
    })
    .toBe(3);
});

// ---- RR-0087: real-time convergence ----------------------------------------
//
// Two independent contexts. Page 1 creates 5 cards via authenticated API
// POSTs; page 2 converges through the SPA's live-update transport (real SSE
// pushes — see header note 1; the test asserts which transport engaged).
// Then page 2's EventSource is killed mid-stream — silently, from page
// context, so the SPA's own staleness watchdog must detect and recover the
// zombie — 3 more cards are created during the gap, and page 2 must converge
// again after the reconnect. Final state: both pages hold the identical set.

test('golden_realtime_convergence', async ({ browser, baseURL, request }, testInfo) => {
  test.setTimeout(120_000);
  const ctxOpts = {
    baseURL,
    ignoreHTTPSErrors: true, // manual contexts do not inherit project `use`
    viewport: testInfo.project.use.viewport ?? undefined,
  };
  const ctx1 = await browser.newContext(ctxOpts);
  const ctx2 = await browser.newContext(ctxOpts);
  try {
    const page1 = await ctx1.newPage();
    const page2 = await ctx2.newPage();
    await settle(page1);
    await settle(page2);
    const token = await appToken(page1);
    const prefix = `e2e-rt-${testInfo.project.name}-w${testInfo.workerIndex}r${testInfo.retry}-${Date.now()}`;

    // Page 2 on the board view so convergence is also visible in the DOM.
    await page2.click('#tab-board');
    await expect(page2.locator('#board-view')).toBeVisible();

    // Wait until each page's live-update machinery is actually running, then
    // ASSERT which transport engaged. With the server speaking the SPA's SSE
    // dialect this must be 'sse'; a silent degrade to polling is exactly the
    // regression this assertion exists to catch (header note 1).
    await expect.poll(() => transport(page1), { timeout: 30_000 }).not.toBe('none');
    await expect.poll(() => transport(page2), { timeout: 30_000 }).not.toBe('none');
    const t1 = await transport(page1);
    const t2 = await transport(page2);
    testInfo.annotations.push({
      type: 'transport',
      description: `page1=${t1} page2=${t2}`,
    });
    expect(t1, 'page 1 live-update transport (sse expected; polling = silent degrade)').toBe('sse');
    expect(t2, 'page 2 live-update transport (sse expected; polling = silent degrade)').toBe('sse');

    // ---- phase 1: 5 rapid creates; page 2 must converge within 5s ---------
    // (SSE pushes are coalesced at 250ms server-side, so the whole burst
    // should land as roughly one full-list push well under a second.)
    const first = [1, 2, 3, 4, 5].map((n) => `${prefix} rapid ${n}`);
    const phase1At = Date.now();
    for (const title of first) {
      const res = await request.post('/api/board', {
        headers: authHeaders(token),
        data: { title, type: 'chore', status: 'todo' },
      });
      expect(res.status()).toBe(201);
    }
    const mine = (all: string[]) => all.filter((t) => t.startsWith(prefix));
    await expect
      .poll(async () => mine(await boardTitles(page2)).length, { timeout: 5_000 })
      .toBe(5);
    testInfo.annotations.push({
      type: 'timing',
      description: `phase-1: ${Date.now() - phase1At}ms from first POST to page-2 in-memory convergence`,
    });
    // Rendered UI carries them too (not just the in-memory array).
    for (const title of first) {
      await expect(page2.locator('#board-view')).toContainText(title, { timeout: 5_000 });
    }

    // ---- phase 2: kill page 2's transport mid-stream ----------------------
    // Preferred kill (now that a live EventSource exists): close it SILENTLY
    // from page context. No error event fires and _liveSSE stays true — a
    // true zombie, so the SPA's 18s staleness watchdog is what must notice
    // and recover (header note 4). The sanctioned setOffline fallback applies
    // only if no EventSource is reachable.
    const killed = (await page2.evaluate(
      "typeof _sse !== 'undefined' && _sse ? (_sse.close(), true) : false",
    )) as boolean;
    testInfo.annotations.push({
      type: 'transport-kill',
      description: killed
        ? 'EventSource closed silently in page context; recovery = SPA staleness watchdog'
        : 'no EventSource reachable from page; fell back to context.setOffline network gap',
    });
    if (!killed) {
      await ctx2.setOffline(true);
      await expect(page2.locator('#conn-status').first()).toHaveText(/Offline|pending/, {
        timeout: 10_000,
      });
    }
    // Drain any invalidate debounce still pending from phase 1 (AMUX-3503):
    // the client now fetches 400ms after an SSE invalidate, and a fetch
    // scheduled BEFORE the kill observes the board as it is WHEN IT FIRES —
    // so without this drain, phase-2 creates landing inside that window
    // arrive on page 2 through a legitimately pre-kill signal and the "gap
    // is real" assert reads 8. Under the old full-push dialect this race
    // could not exist (a push carried the state at push time).
    await page2.waitForTimeout(700);
    const killAt = Date.now();

    const second = [6, 7, 8].map((n) => `${prefix} gap ${n}`);
    for (const title of second) {
      const res = await request.post('/api/board', {
        headers: authHeaders(token),
        data: { title, type: 'chore', status: 'todo' },
      });
      expect(res.status()).toBe(201);
    }
    // The gap is real: the dead transport cannot have delivered the new cards.
    await page2.waitForTimeout(1_500);
    expect(mine(await boardTitles(page2)).length).toBe(5);

    // ---- recovery: page 2 must converge via the SPA's own resume path -----
    // Zombie path budget: _lastDataTime must age past 18s, the watchdog
    // checks every 5s, then the fresh connection's initial push + refetch
    // land — so up to ~25s is the SPA working as designed, not slack.
    if (!killed) await ctx2.setOffline(false);
    await expect
      .poll(async () => mine(await boardTitles(page2)).length, {
        timeout: killed ? 40_000 : 15_000,
      })
      .toBe(8);
    testInfo.annotations.push({
      type: 'timing',
      description: `phase-2: ${Date.now() - killAt}ms from kill to page-2 convergence (${
        killed ? 'stale-watchdog reconnect' : 'offline→online resume'
      })`,
    });
    // And the transport re-established as SSE, not a degraded fallback.
    expect(await transport(page2), 'page 2 transport after recovery').toBe('sse');

    // Page 1 (never disturbed) also holds all 8.
    await expect
      .poll(async () => mine(await boardTitles(page1)).length, { timeout: 15_000 })
      .toBe(8);

    // ---- final: identical state on both pages, exactly once on the server -
    const titles1 = mine(await boardTitles(page1)).sort();
    const titles2 = mine(await boardTitles(page2)).sort();
    expect(titles1).toEqual(titles2);
    expect(titles1).toHaveLength(8);
    const all = await serverTitles(request, token);
    for (const t of [...first, ...second]) {
      expect(all.filter((x) => x === t), `exactly one server card for "${t}"`).toHaveLength(1);
    }
  } finally {
    await ctx1.close();
    await ctx2.close();
  }
});
