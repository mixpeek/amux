/**
 * The fixture's own test: does a dead `page.route` stub actually get noticed?
 *
 * e2e/fixtures.ts exists because a stub that matches zero requests is silent
 * (AF-47). `crates/amux-server/tests/e2e_route_stub_guard.rs` proves every spec
 * that stubs imports the fixture — but importing it is not the same as the
 * fixture WORKING, and a guard that only checks the import would be green over
 * a wrapper that counted nothing. This file tests the wrapper.
 *
 * All three cells run against the real Playwright runner and the real fixture,
 * because the defect lives in the teardown path and nothing above it flows
 * through that path.
 */
import { test, expect, allowUnusedRoute } from './fixtures';

// CELL 1 — the whole point. `test.fail()` inverts the expectation, so this cell
// passes only if the run FAILS, and the only thing that can fail it is the
// fixture's teardown: the body does nothing that can throw.
test('a stub that matches zero requests fails the test', async ({ page }) => {
  test.fail(true, 'the fixture must reject this run at teardown (AF-47)');
  await page.route('**/api/never-requested-by-any-page', (r) =>
    r.fulfill({ contentType: 'application/json', body: '{}' }));
  await page.goto('/');
});

// CELL 2 — the opt-out must actually opt out, or the escape hatch is decorative
// and the first spec that needs it reaches for something worse.
test('a stub declared with allowUnusedRoute does not fail', async ({ page }) => {
  const pattern = '**/api/also-never-requested';
  await page.route(pattern, (r) =>
    r.fulfill({ contentType: 'application/json', body: '{}' }));
  allowUnusedRoute(page, pattern);
  await page.goto('/');
});

// CELL 3 — the control. Without it, cell 1 is equally consistent with "the
// fixture fails EVERY test that registers a route", which would be a wrapper
// that breaks all four real stubs while looking like it works.
test('a stub that DOES match does not fail', async ({ page }) => {
  let hits = 0;
  await page.route('**/api/sessions*', async (r) => { hits += 1; await r.continue(); });
  await page.goto('/');
  await page.waitForResponse((r) => r.url().includes('/api/sessions'), { timeout: 20000 });
  expect(hits).toBeGreaterThan(0);
});
