// Ethan, 2026-10-23: on the worker page, everything between the folder line and
// a tab's content is ONE row. Notices and explanations live behind an info
// button at the end of that row (.row-note) and open over the content.
import { test, expect } from './fixtures';
import { cleanup } from './teardown';

const TABS = ['steering', 'schedules', 'messages', 'issues', 'memory', 'tasks', 'shell', 'cost'];

test('every worker tab has one control row above its content', async ({ page, request }, testInfo) => {
  test.setTimeout(120_000);
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
  const auth = { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' };
  const name = `onerow-${testInfo.project.name}-${Date.now()}`;
  const created = await request.post('/api/sessions', { headers: auth, data: { name, dir: '/tmp' } });
  expect(created.status()).toBe(201);
  try {
    await page.reload();
    await page.waitForFunction(n => typeof (window as any).openPeek === 'function'
      && (globalThis as any).eval('typeof sessions') !== 'undefined'
      && (globalThis as any).eval('sessions').some((s: any) => s.name === n), name);
    await page.evaluate(n => (window as any).openPeek(n), name);
    const report: string[] = [];
    for (const tab of TABS) {
      await page.evaluate(t => (window as any).setPeekTab(t), tab);
      await page.waitForTimeout(600);
      // The rows of controls: the panel's visible children above the first one
      // tall enough to be content (or the end of the panel).
      const rows = await page.evaluate(() => {
        const panel = [...document.querySelectorAll('#peek-overlay [id^="peek-"][id$="-panel"]')]
          .filter((p: any) => p.offsetParent && p.getBoundingClientRect().height > 100)
          .sort((a, b) => b.getBoundingClientRect().height - a.getBoundingClientRect().height)[0] as HTMLElement | undefined;
        if (!panel) return null;
        const out: { id: string; h: number }[] = [];
        for (const c of Array.from(panel.children) as HTMLElement[]) {
          const r = c.getBoundingClientRect();
          if (!r.height || getComputedStyle(c).display === 'none' || getComputedStyle(c).position === 'absolute') continue;
          // Content: anything tall, or the tab's list (which is short when empty).
          if (r.height > 120 || /-list$/.test(c.id) || /(^| )(peek-tasks-list|tasks-list)( |$)/.test(String(c.className))) break;
          out.push({ id: c.id || String(c.className).split(' ')[0] || c.tagName, h: Math.round(r.height) });
        }
        return { panel: panel.id, out };
      });
      expect(rows, `${tab}: a panel is showing`).not.toBeNull();
      report.push(`${tab} [${rows!.panel}]: ${rows!.out.map(r => `${r.id} ${r.h}px`).join(', ')}`);
      expect(rows!.out.length, `${tab}: one control row, got ${JSON.stringify(rows!.out)}`).toBeLessThanOrEqual(1);
      if (rows!.out.length) expect(rows!.out[0].h, `${tab}: the row does not wrap`).toBeLessThanOrEqual(72);   // 44px targets + padding; a wrapped row is over 100px
    }
    testInfo.annotations.push({ type: 'rows', description: report.join(' | ') });

    // The Tasks note opens over the list and closes on Escape.
    await page.evaluate(() => (window as any).setPeekTab('tasks'));
    const note = page.locator('#peek-tasks-panel .row-note');
    await note.locator('summary').click();
    await expect(note.locator('.row-note-body')).toBeVisible();
    await expect(note.locator('.row-note-body')).toContainText('These are tasks, not board issues.');
    await page.keyboard.press('Escape');
    await expect(note.locator('.row-note-body')).toBeHidden();
  } finally {
    await cleanup('delete fixture worker', () => request.delete(`/api/sessions/${encodeURIComponent(name)}`, { headers: auth }), testInfo);
  }
});
