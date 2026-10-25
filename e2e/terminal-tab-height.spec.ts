// The Terminal tab keeps one height (Ethan, 2026-10-21: "it still does this
// weird infinite height thing"). The view was shown as display:block, so the
// container's flex:1 never applied and xterm's fit read back its own content
// height; and _termLayout sized from the viewport top, which shrinks as the
// page scrolls. Each resize then added ~6 rows and ~96px of page. The PTY
// routes are fakes: no shell is started.
import { test, expect, allowUnusedRoute } from './fixtures';

test('the Terminal tab does not grow on scroll and resize', async ({ page }) => {
  await page.route('**/api/terminal/create', r => r.fulfill({ json: { id: 'pty-fake' } }));
  await page.route('**/api/terminal/pty-fake/output**', async r => { await new Promise(res => setTimeout(res, 1000)); await r.fulfill({ json: { alive: true, data: '' } }); });
  await page.route('**/api/terminal/pty-fake/resize', r => r.fulfill({ json: { ok: true } }));
  allowUnusedRoute(page, '**/api/terminal/pty-fake/resize');
  await page.route('**/api/terminal/pty-fake', r => r.fulfill({ json: { ok: true } }));
  allowUnusedRoute(page, '**/api/terminal/pty-fake');
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction("typeof switchView === 'function' && typeof _termConnect === 'function'");
  await page.evaluate("switchView('terminal')");
  await page.evaluate('_termConnect()');
  await page.waitForFunction('_termId && _term');
  await page.waitForTimeout(500);
  const m = () => page.evaluate(`({ view: document.getElementById('terminal-view').getBoundingClientRect().height,
    doc: document.documentElement.scrollHeight, inner: innerHeight, rows: _term.rows,
    display: getComputedStyle(document.getElementById('terminal-view')).display })`) as Promise<any>;
  const start = await m();
  expect(start.display).toBe('flex');
  const vp = page.viewportSize()!;
  for (let i = 0; i < 5; i++) {
    await page.evaluate('window.scrollTo(0, document.documentElement.scrollHeight)');
    await page.setViewportSize({ width: vp.width, height: vp.height - (i % 2) });
    await page.waitForTimeout(350);
  }
  await page.setViewportSize(vp);
  await page.waitForTimeout(350);
  const end = await m();
  expect(Math.abs(end.rows - start.rows), JSON.stringify({ start, end })).toBeLessThanOrEqual(1);
  expect(Math.abs(end.view - start.view), JSON.stringify({ start, end })).toBeLessThanOrEqual(2);
  expect(end.doc - end.inner, JSON.stringify({ start, end })).toBeLessThanOrEqual(start.doc - start.inner + 2);
});
