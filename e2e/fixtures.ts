/**
 * The e2e `test` every spec should import: `page.route` stubs that never match
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
 * WHAT IT DOES. `page.route` is wrapped so every registered stub counts its
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
import type { Page } from '@playwright/test';

export { expect };
export type { Page };

type Stub = { pattern: string; hits: number };

const STATE = new WeakMap<Page, { stubs: Stub[]; allowed: Set<string> }>();

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
export function allowUnusedRoute(page: Page, url: string | RegExp): void {
  const st = STATE.get(page);
  if (!st) throw new Error(
    'allowUnusedRoute() before any page.route() on this page — nothing to allow. ' +
    'Register the stub first, then declare it may not fire.',
  );
  st.allowed.add(describeMatcher(url));
}

export const test = base.extend<{ page: Page }>({
  page: async ({ page }, use, testInfo) => {
    const st = { stubs: [] as Stub[], allowed: new Set<string>() };
    STATE.set(page, st);

    const native = page.route.bind(page);
    // Count hits on the WRAPPER, not by inspecting Playwright's internals: the
    // handler we register is the only place that provably runs when a request
    // matched, and it is stable across Playwright versions.
    (page as unknown as { route: unknown }).route = (
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

    await use(page);

    // THE REAL ERROR WINS. See the header: an unhit stub is usually downstream
    // of whatever actually broke, and naming it would point at the wrong thing.
    if (testInfo.errors.length) return;

    const dead = st.stubs.filter((s) => s.hits === 0 && !st.allowed.has(s.pattern));
    if (!dead.length) return;

    const lines = dead.map((s) => `    ${s.pattern}`).join('\n');
    throw new Error(
      `${dead.length} page.route stub(s) matched ZERO requests:\n${lines}\n\n` +
      '  The test therefore exercised the REAL endpoint, not your stub, and any\n' +
      '  assertion about the response was made against live data. This is not a\n' +
      '  rendering bug — check the stub before you check the renderer (AF-47).\n\n' +
      '  Usual causes: the glob does not match the URL the app actually requests;\n' +
      '  a service worker served the request (playwright.config.ts blocks workers\n' +
      '  by default, so a spec that opted back in with serviceWorkers: \'allow\'\n' +
      '  is the first thing to check); the route was registered after the request\n' +
      '  was issued; the app no longer calls that path.\n\n' +
      '  If it legitimately may not fire, say so at the call site:\n' +
      '      allowUnusedRoute(page, <the same matcher>);  // and why',
    );
  },
});
