/**
 * The e2e `test` every spec should import: page.route stubs that never match
 * are reported instead of passing silently (AF-47).
 *
 * WHY THIS EXISTS, measured 2026-08-13. `system-jobs.spec.ts` stubbed
 * `/api/system-jobs` with `page.route` and a registered service worker defeated
 * it — the request went through the worker's fetch handler, where `page.route`
 * cannot see it. The stub did not error. The page rendered the REAL job list,
 * the spec diffed that against the stubbed shape, and the failure read as "the
 * stalled-row styling is broken under WebKit". The natural next move is to go
 * read the CSS, and that is where the hour went.
 *
 * The service-worker half is fixed: playwright.config.ts sets
 * `serviceWorkers: 'block'` as the default, opt-OUT. This file fixes the other
 * half, which is the general one — NOTHING tells you a stub matched zero
 * requests. A stub that never fires is almost always a bug, and today it is
 * indistinguishable from one that fired: same green-looking machinery, no
 * output either way. A service worker is only one of the ways to get there; a
 * pattern with the wrong glob, a path that changed, a request the app stopped
 * making, and a route registered after the request all produce the identical
 * silence and the identical confident-but-wrong failure about rendering.
 *
 * WHAT IT DOES. Every page.route is wrapped so every registered stub counts its
 * hits, and at teardown a stub with zero hits fails the test with a message
 * that names the stub — so the next occurrence says "this stub never matched,
 * you tested the REAL endpoint" rather than blaming the renderer.
 *
 * IT IS SILENT WHEN THE TEST ALREADY FAILED. An unhit stub is a common
 * DOWNSTREAM symptom of a test that died before it got to the interaction, and
 * reporting it there would be this entry's own defect committed by its own fix:
 * a confident message pointing at the wrong subsystem. The real error wins.
 *
 * OPTING OUT. A stub that legitimately may not fire — a defensive one for a
 * branch that does not always run — declares itself:
 *
 *     await page.route('**\/api/thing', handler);
 *     allowUnusedRoute(page, '**\/api/thing');   // and say WHY on this line
 *
 * The declaration is the point. "May not fire" is a claim about the app, and
 * writing it down is what stops the guard from being switched off wholesale.
 */
import { test as base, expect } from '@playwright/test';
import type { BrowserContext, Page, TestInfo } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';

// Browser-only checks can use a prebuilt API server and explicitly pin the
// candidate asset. The digest records exactly which bytes the run exercised.
const dashboardAssets = [
  ['AMUX_E2E_DASHBOARD_SOURCE', '**/app.js', 'text/javascript'],
  ['AMUX_E2E_DASHBOARD_CSS', '**/app.css', 'text/css'],
  ['AMUX_E2E_DASHBOARD_HTML', '**/', 'text/html'],
  ['AMUX_E2E_STATE_KERNEL', '**/state/kernel.js', 'text/javascript'],
  // The kernel fetches the command registry at runtime; pin a candidate one
  // alongside a candidate kernel when the handlers changed.
  ['AMUX_E2E_CONTROL_REGISTRY', '**/state/control-registry.json', 'application/json'],
].flatMap(([env, route, contentType]) => {
  const source = process.env[env];
  if (!source) return [];
  const bytes = readFileSync(source);
  console.log('[e2e] candidate dashboard asset: ' + source + ' sha256='
    + createHash('sha256').update(bytes).digest('hex') + '; API binary is unchanged');
  return [{route: route === '**/' ? /^https?:\/\/[^/]+\/(?:\?.*)?$/ : route, bytes, contentType}];
});

export { expect };
export type { Page };

// THIRD-PARTY CDN ASSETS GATE THE PAGE "load" EVENT. index.html pulls about
// twenty stylesheets and scripts from jsdelivr, unpkg and cdnjs, and
// page.goto('/') waits for "load", which waits for every one of them. When one
// CDN request stalls, the dashboard renders and works but "load" never fires,
// so the test dies in page.goto at its own timeout with nothing pointing at the
// network. Measured on main: run 38936517715 (settings.spec.ts:560, ios-safari)
// has a trace whose only unfinished request is cdnjs's github-dark.min.css, and
// run 38848402875 (peek-split-view-memory.spec.ts:170, desktop) shows the same
// page.goto "waiting until load" timeout over a fully rendered dashboard.
//
// So each Playwright worker fetches a CDN URL once and serves later requests
// from memory, which removes most of the live CDN traffic a shard makes, and a
// fetch that has no answer after CDN_FETCH_TIMEOUT_MS is aborted with a
// `[e2e] cdn-stall` line naming the URL. The abort lets "load" fire; a test
// that genuinely needs the stalled library then fails on that, next to a log
// line that says why, rather than as an unexplained navigation timeout.
const CDN_URL = /^https:\/\/(?:cdn\.jsdelivr\.net|unpkg\.com|cdnjs\.cloudflare\.com)\//;
const CDN_FETCH_TIMEOUT_MS = 10_000;
type CdnEntry = { status: number; headers: Record<string, string>; body: Buffer };
const cdnCache = new Map<string, CdnEntry>();
const cdnStats = { fetched: 0, served_from_cache: 0, stalled: 0 };
process.on('exit', () => {
  if (cdnStats.fetched + cdnStats.served_from_cache + cdnStats.stalled === 0) return;
  console.log('[e2e] cdn cache: ' + JSON.stringify(cdnStats));
});

type Stub = { pattern: string; hits: number };
type RouteOwner = Page;
type RouteState = { stubs: Stub[]; allowed: Set<string>; label: string };
const STATE = new WeakMap<RouteOwner, RouteState>();

/** Render a route matcher the way the spec wrote it, for the failure message. */
function describeMatcher(url: unknown): string {
  if (typeof url === 'string') return url;
  if (url instanceof RegExp) return String(url);
  return '<predicate function>';
}

/**
 * Declare that a stub may legitimately never fire. Pass the SAME matcher you
 * passed to `page.route`; it is compared by its rendered form, so a string
 * matches the identical string and a RegExp matches an identical RegExp.
 */
export function allowUnusedRoute(owner: RouteOwner, url: string | RegExp): void {
  const st = STATE.get(owner);
  if (!st) throw new Error(
    'allowUnusedRoute() before this page was wrapped — nothing to allow. ' +
    'Register the stub first, then declare it may not fire.',
  );
  st.allowed.add(describeMatcher(url));
}

// Wrap every page the context creates, including context.newPage().
// Candidate asset plumbing is installed first; only test-declared stubs count.
function trackRoutes(owner: RouteOwner, label: string): RouteState {
  const existing = STATE.get(owner);
  if (existing) return existing;
  const st: RouteState = { stubs: [], allowed: new Set(), label };
  STATE.set(owner, st);
  const native = owner.route.bind(owner);
  // Count hits on the WRAPPER, not by inspecting Playwright's internals: the
  // handler we register is the only place that provably runs when a request
  // matched, and it is stable across Playwright versions.
  (owner as unknown as { route: unknown }).route = (
    url: Parameters<Page['route']>[0],
    handler: Parameters<Page['route']>[1],
    options?: Parameters<Page['route']>[2],
  ) => {
    const stub: Stub = { pattern: describeMatcher(url), hits: 0 };
    st.stubs.push(stub);
    return native(
      url,
      (route, request) => {
        stub.hits += 1;
        return (handler as (r: typeof route, q: typeof request) => unknown)(route, request);
      },
      options,
    );
  };

  return st;
}

function assertRoutesUsed(st: RouteState): void {
  const dead = st.stubs.filter((s) => s.hits === 0 && !st.allowed.has(s.pattern));
  if (!dead.length) return;

  const lines = dead.map((s) => `    ${s.pattern}`).join('\n');
  throw new Error(
    `${dead.length} ${st.label}.route stub(s) matched ZERO requests:\n${lines}\n\n` +
    '  The test therefore exercised the REAL endpoint, not your stub, and any\n' +
    '  assertion about the response was made against live data. This is not a\n' +
    '  rendering bug — check the stub before you check the renderer (AF-47).\n\n' +
    '  Usual causes: the glob does not match the URL the app actually requests;\n' +
    '  a service worker served the request (playwright.config.ts blocks workers\n' +
    '  by default, so a spec that opted back in with serviceWorkers: \'allow\'\n' +
    '  is the first thing to check); the route was registered after the request\n' +
    '  was issued; the app no longer calls that path.\n\n' +
    '  If it legitimately may not fire, say so at the call site:\n' +
    `      allowUnusedRoute(${st.label}, <the same matcher>);  // and why`,
  );
}

// First-run surfaces off for every context a test opens, including the ones a
// spec makes itself with browser.newContext (golden_realtime_convergence, ...).
// Those used to skip this, so a phone-width second context showed the Cloud
// offer and wrote the server-wide cloud_offer pref while cloud-offer.spec.ts,
// in the other worker, had just reset it (run 39968514404).
async function seedFirstRunOff(context: BrowserContext, testInfo: TestInfo): Promise<void> {
  // FIRST-RUN WALKTHROUGH OFF unless a test asks for it. The scratch server
  // starts with no workers, and the walkthrough correctly opens for an empty
  // fleet ~1.5s after load (_wtMaybeStart). On slower ios-safari runners it
  // opened before the test's first click and #wt-backdrop swallowed it:
  // browser-history, message-context-filter and mobile-header-visibility
  // timed out on main (run 36921271354). Opt in with
  // test.info().annotations.push({type:'walkthrough'}) set before navigation,
  // or {annotation:{type:'walkthrough'}} on the test.
  if (!testInfo.annotations.some(a => a.type === 'walkthrough')) {
    await context.addInitScript(() => {
      try { if (!localStorage.getItem('amux_walkthrough_done')) localStorage.setItem('amux_walkthrough_done', '1'); } catch (_) {}
    });
  }
  // amux CLOUD OFFER OFF unless a test asks for it, for the walkthrough's
  // reason: every e2e server is self-hosted and owned, so the first phone-width
  // page would get a fixed card over the bottom of the screen. Opt in with
  // {annotation:{type:'cloud-offer'}} (e2e/cloud-offer.spec.ts).
  if (!testInfo.annotations.some(a => a.type === 'cloud-offer')) {
    await context.addInitScript(() => {
      try { if (!localStorage.getItem('amux_cloud_offer')) localStorage.setItem('amux_cloud_offer', '{"dismissed":1}'); } catch (_) {}
    });
  }
}

export const test = base.extend<{ _seedNewContexts: void }>({
  // browser is per worker and a worker runs one test at a time, so wrapping
  // newContext for the length of a test cannot leak into another test.
  _seedNewContexts: [async ({ browser }, use, testInfo) => {
    const original = browser.newContext;
    browser.newContext = async function (this: typeof browser, ...args: Parameters<typeof original>) {
      const ctx = await original.apply(this, args);
      await seedFirstRunOff(ctx, testInfo);
      return ctx;
    } as typeof original;
    try { await use(); } finally { browser.newContext = original; }
  }, { auto: true }],
  context: async ({ context }, use, testInfo) => {
    await seedFirstRunOff(context, testInfo);
    await context.route(CDN_URL, async route => {
      const url = route.request().url();
      const hit = cdnCache.get(url);
      if (hit) {
        cdnStats.served_from_cache++;
        return route.fulfill(hit);
      }
      const started = Date.now();
      let response;
      try {
        response = await route.fetch({ timeout: CDN_FETCH_TIMEOUT_MS });
      } catch (e) {
        cdnStats.stalled++;
        console.log(`[e2e] cdn-stall: ${url} gave no response in ${Date.now() - started}ms `
          + `(${String(e).split('\n')[0]}); aborting it so the page "load" event still fires `
          + `(test: ${testInfo.titlePath.join(' > ')})`);
        return route.abort('timedout').catch(() => {});
      }
      cdnStats.fetched++;
      // The fetched body is already decoded; drop the transfer headers that
      // describe the encoded bytes.
      const headers = { ...response.headers() };
      for (const h of ['content-encoding', 'content-length', 'transfer-encoding']) delete headers[h];
      // A library the page asks for on first use (_lazyLib) can still be in
      // flight when the test ends and its context closes. The body is gone
      // then, and nobody is waiting for it, so that is not a test failure.
      let body: Buffer;
      try { body = await response.body(); }
      catch (e) {
        console.log(`[e2e] cdn-disposed: ${url} finished after its page closed (test: ${testInfo.titlePath.join(' > ')})`);
        return;
      }
      const entry = { status: response.status(), headers, body };
      if (response.ok()) cdnCache.set(url, entry);
      return route.fulfill(entry).catch(() => {});
    });
    // A second tab must run the same candidate asset as the first. Page-only
    // overrides silently mixed the candidate outbox with the installed one.
    for (const asset of dashboardAssets) {
      await context.route(asset.route, async route => {
        if (asset.contentType !== 'text/html') return route.fulfill({body:asset.bytes, contentType:asset.contentType});
        // The server injects the real isolated home's bootstrap credentials.
        // Replacing HTML without them tests a refused/auto-reloading shell.
        if (route.request().resourceType() !== 'document') return route.continue();
        const response = await route.fetch({timeout:15000});
        const served = await response.text();
        const bootstrap = served.match(/<script>window\._AMUX_S3_ICAL_URL=[\s\S]*?<\/script>/);
        if (!bootstrap) throw new Error('candidate HTML: server bootstrap was not measured');
        const body = asset.bytes.toString().replace(/<script>window\._AMUX_S3_ICAL_URL=[\s\S]*?<\/script>/, () => bootstrap[0]);
        await route.fulfill({response, body, contentType:asset.contentType});
      });
    }
    const tracked: RouteState[] = [];
    const trackPage = (page: Page) => tracked.push(trackRoutes(page, 'page'));
    for (const page of context.pages()) trackPage(page);
    context.on('page', trackPage);
    try {
      await use(context);
    } finally {
      context.off('page', trackPage);
    }
    // THE REAL ERROR WINS. A dead stub after an earlier error is downstream.
    if (testInfo.errors.length) return;
    for (const st of tracked) assertRoutesUsed(st);
  },
});
