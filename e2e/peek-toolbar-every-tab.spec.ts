// The worker-view toolbar works on every tab (Ethan, 2026-10-24: "these should
// have the same header and i should be able to scroll thru messages and search
// filter etc for chat just like terminal ... like schedule i should be able to
// search across runs"). Each tab's panel is filled with rows in the markup that
// tab renders (sampled from the live dashboard), then the toolbar is driven
// through its real buttons and Find box.
import { test, expect, Page } from './fixtures';

async function openOn(page: Page, tab: string) {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function'
    && (globalThis as any).eval('typeof sessions') !== 'undefined');
  await page.evaluate(() => (globalThis as any).eval(
    "sessions.push({name:'zz-tabs',dir:'/tmp',running:true,status:'idle'}); openPeek('zz-tabs')"));
  await expect(page.locator('#peek-overlay')).toBeVisible();
  await page.evaluate(t => (window as any).setPeekTab(t), tab);
}
// The tab's own loader may repaint once; fill after it settles, then wait for
// the toolbar to have re-applied (it watches the panel).
async function fill(page: Page, selector: string, html: string) {
  await page.waitForTimeout(400);
  await page.evaluate(([sel, h]) => { (document.querySelector(sel) as HTMLElement).innerHTML = h; }, [selector, html] as const);
}
const find = async (page: Page, q: string) => {
  if (await page.locator('#peek-search-wrap').isHidden()) await page.locator('#peek-find-toggle').click();
  await page.locator('#peek-search').fill(q);
};

test('Schedules: Find searches across runs, up/down steps through them, the count follows', async ({ page }) => {
  const beacons: any[] = [];
  page.on('request', r => { if (r.url().includes('/api/client-debug') && r.method() === 'POST') { try { const b = r.postDataJSON(); if (b.kind === 'peek-tab-find') beacons.push(b); } catch {} } });
  await openOn(page, 'schedules');
  const rows = ['nightly backup ok', 'deploy amux failed: rustc', 'deploy amux ok', 'digest sent', 'deploy mixpeek ok']
    .map((t, i) => `<div class="sx-fleet-run" data-i="${i}" style="height:60px">${t}</div>`).join('');
  await fill(page, '#peek-schedules-panel', `<div style="height:2000px"></div><details id="zz-runs"><summary>Recent Runs</summary>${rows}</details>`);
  await expect(page.locator('#peek-msg-nav')).toBeVisible();
  await expect(page.locator('#peek-find-toggle')).toBeVisible();
  await find(page, 'deploy');
  await expect(page.locator('#peek-schedules-panel .sx-fleet-run:not(.peek-tab-miss)')).toHaveCount(3);
  await expect(page.locator('#zz-runs'), 'a collapsed Recent Runs opens to its matches').toHaveAttribute('open', '');
  await expect(page.locator('#peek-msg-count')).toHaveText('3');
  await expect.poll(() => beacons.some(b => b.tab === 'schedules' && b.searching && b.matches === 3), { message: 'the search reports itself' }).toBe(true);
  await expect(page.locator('#peek-search')).toHaveAttribute('placeholder', 'Find in schedules and runs…');
  await expect(page.locator('#peek-schedules-panel .peek-highlight').first()).toHaveText('deploy');
  await page.locator('#peek-overlay .peek-nav-btn').last().click();   // down
  await expect(page.locator('#peek-msg-count')).toHaveText('1/3');
  const cur = page.locator('#peek-schedules-panel .peek-tab-current');
  await expect(cur).toContainText('deploy amux failed');
  await expect(cur).toBeInViewport();
  await page.locator('#peek-overlay .peek-nav-btn').last().click();
  await expect(cur).toContainText('deploy amux ok');
  await page.locator('#peek-overlay .peek-nav-btn').first().click();   // up
  await expect(cur).toContainText('deploy amux failed');
  // A repaint by the tab's own poll keeps the search applied, and the place.
  await fill(page, '#peek-schedules-panel', rows + '<div class="sx-fleet-run">deploy canvas ok</div>');
  await expect(page.locator('#peek-msg-count')).toHaveText('1/4', { timeout: 3000 });
  await expect(page.locator('#peek-schedules-panel .sx-fleet-run.peek-tab-miss')).toHaveCount(2);
  // Clearing the search shows every run again.
  await page.locator('#peek-search-wrap .search-clear').click();
  await expect(page.locator('#peek-schedules-panel .peek-tab-miss')).toHaveCount(0);
});

test('Chat: the filter keeps messages with files, and Find works inside chat', async ({ page }) => {
  await openOn(page, 'chat');
  const msgs = [
    'plain hello',
    'see <span class="file-link" data-path="/tmp/a.md">/tmp/a.md</span>',
    'board item AMUX-12 is done',
    'another plain one',
  ].map(t => `<div class="chat-msg"><div class="chat-bubble">${t}</div></div>`).join('');
  const root = await page.evaluate(() => document.querySelector('#peek-body.peek-chat') ? '#peek-body' : '#peek-chat-panel');
  await fill(page, root, msgs);
  await expect(page.locator('#peek-msg-nav')).toBeVisible();
  await page.locator('#peek-filter-btn').click();
  await page.locator('[name="peek-filter-content"][value="files"]').check();
  await expect(page.locator(`${root} .chat-msg:not(.peek-tab-miss)`)).toHaveCount(1);
  await expect(page.locator(`${root} .chat-msg:not(.peek-tab-miss)`)).toContainText('/tmp/a.md');
  await expect(page.locator('#peek-msg-count')).toHaveText('1');
  await page.locator('#peek-filter-reset').click();
  await page.keyboard.press('Escape');
  await expect(page.locator(`${root} .chat-msg.peek-tab-miss`)).toHaveCount(0);
  await find(page, 'plain');
  await expect(page.locator('#peek-msg-count')).toHaveText('2');
  await page.locator('#peek-search').press('Enter');
  await expect(page.locator(`${root} .peek-tab-current`)).toContainText('plain hello');
});

test('a tab with no list still gets Find: matches highlight and up/down steps between them', async ({ page }) => {
  await openOn(page, 'steering');
  await fill(page, '#peek-steering-panel', '<p>keep the brand voice</p><p>no brand colours in code</p><p>other</p>');
  await find(page, 'brand');
  await expect(page.locator('#peek-steering-panel .peek-highlight')).toHaveCount(2);
  await expect(page.locator('#peek-msg-count')).toHaveText('2');
  await page.locator('#peek-overlay .peek-nav-btn').last().click();
  await expect(page.locator('#peek-steering-panel .peek-highlight.current')).toHaveCount(1);
  await expect(page.locator('#peek-msg-count')).toHaveText('1/2');
});

test('switching back to the terminal gives Find back to the terminal', async ({ page }) => {
  await openOn(page, 'schedules');
  await page.evaluate(() => (window as any).setPeekTab('terminal'));
  await expect(page.locator('#peek-search')).toHaveAttribute('placeholder', 'Find in terminal…');
  await page.evaluate(() => (window as any).setPeekTab('logs'));
  await expect(page.locator('#peek-search')).toHaveAttribute('placeholder', 'Find in logs…');
});
