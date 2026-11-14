import {test, expect} from './fixtures';

// Owner, 2026-11-13: "if I have one tab and I'm in a specific worker, and I go
// to the other tab ... that tab updates to the worker I was in from the other
// tab." Every tab of the origin shares localStorage, so amux_peek_state and
// amux_ui_view hold whatever tab was used LAST. A tab whose address names no
// screen (the Workers list and the Files root have no #view=/#peek=) that
// reloaded (Chrome discarding a background tab, a deploy's reload-when-shown)
// came back on the other tab's worker. A reload now restores the tab's own
// state (sessionStorage); the shared copy is only for a tab that has none.
test('a reloaded tab keeps its own screen, not the worker another tab opened last', async ({page}) => {
  await page.context().route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json: [
    {name: 'tab-a-worker', running: true, status: 'idle', dir: '/tmp'},
    {name: 'tab-b-worker', running: true, status: 'idle', dir: '/tmp'},
  ]}));
  const ready = (p: typeof page) => p.waitForFunction("typeof openPeek === 'function' && typeof switchView === 'function'");
  const a = await page.context().newPage();
  const b = page;
  await a.goto('/'); await ready(a);
  await b.goto('/'); await ready(b);

  for (const view of ['sessions', 'files']) {
    // B sits on a screen whose address is empty.
    await b.evaluate(v => { if ((window as any).peekSession) (window as any).closePeek(); (window as any).switchView(v); }, view);
    await expect.poll(() => b.evaluate('activeView')).toBe(view);
    // A, used last, opens a worker: the shared copy now names it.
    await a.evaluate(() => (window as any).openPeek('tab-a-worker'));
    await expect.poll(() => a.evaluate("JSON.parse(localStorage.getItem('amux_peek_state') || '{}').session")).toBe('tab-a-worker');

    await b.reload(); await ready(b);
    // The restore fires 200 ms after boot; give it well past that.
    await b.waitForTimeout(1500);
    expect(await b.evaluate('peekSession'), `B on ${view} did not open A's worker`).toBeNull();
    expect(await b.evaluate('activeView'), `B came back on ${view}`).toBe(view);
  }

  // B's own worker survives its reload too.
  await b.evaluate(() => (window as any).openPeek('tab-b-worker'));
  await b.reload(); await ready(b);
  await expect.poll(() => b.evaluate('peekSession'), {timeout: 8000}).toBe('tab-b-worker');
  await a.close();
});
