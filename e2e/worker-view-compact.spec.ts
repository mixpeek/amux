// The worker view is compact on desktop (Ethan, 2026-10-23: "too large a % of
// the overall screen on worker views"). With a mouse, the title and task share
// one band with the toolbar, and "Send command" sits on the status line. Every
// control stays shown and keeps its size. Phones keep the stacked header.
import { test, expect } from './fixtures';

test('the worker view header is one band on desktop and stacked on a phone', async ({ page, request }, testInfo) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any).openPeek === 'function'
    && (globalThis as any).eval('typeof sessions') !== 'undefined');
  await page.evaluate(() => (globalThis as any).eval(
    "localStorage.removeItem('amux_peek_head'); sessions.push({name:'zz-compact',dir:'/tmp',running:true,status:'active',task_name:'Applying namespace manifest to staging'}); openPeek('zz-compact')"));
  await expect(page.locator('#peek-overlay .peek-toolbar')).toBeVisible();
  const g = await page.evaluate(() => {
    const r = (s: string) => { const e = document.querySelector(s) as HTMLElement | null; return e && e.getClientRects().length ? e.getBoundingClientRect() : null; };
    const head = r('#peek-overlay > .overlay-header')!, title = r('#peek-title-row')!, tools = r('#peek-overlay .peek-toolbar')!;
    const status = r('#peek-status'), toggle = r('#peek-cmd-toggle');
    const fine = matchMedia('(min-width: 601px) and (hover: hover) and (pointer: fine)').matches;
    const btns = [...document.querySelectorAll('#peek-overlay .peek-toolbar button')].filter(b => b.getClientRects().length).map(b => b.getBoundingClientRect().height);
    return { fine, head: head.height, sameBand: Math.abs((title.top + title.bottom) / 2 - (tools.top + tools.bottom) / 2) < tools.height / 2 && title.right <= tools.left + 1,
      toggleOnStatus: !!(status && toggle && Math.abs(toggle.top - status.top) < 8), minBtn: Math.min(...btns) };
  });
  testInfo.annotations.push({ type: 'geometry', description: JSON.stringify(g) });
  expect(g.minBtn, 'toolbar controls keep their size').toBeGreaterThanOrEqual(43.9);
  if (g.fine) {
    expect(g.sameBand, 'title and toolbar share one band').toBe(true);
    expect(g.head, 'the header is one band').toBeLessThanOrEqual(62);
    expect(g.toggleOnStatus, '"Send command" sits on the status line').toBe(true);
  } else {
    expect(g.sameBand, 'a touch screen keeps the stacked header').toBe(false);
  }
});

// A Mac app pane is its own page, about 640px wide (Ethan, 2026-10-24
// screenshot: two panes, the task "Update brand reconfiguration plan
// post-launch" one letter per line down the whole pane). The one-band header
// put the toolbar beside the title, leaving the title a letter's width. Where
// the view is too narrow for both, the toolbar goes under the title.
for (const width of [640, 760, 900]) {
  test(`at ${width}px wide the task stays readable and the toolbar fits`, async ({ page }, testInfo) => {
    await page.setViewportSize({ width, height: 900 });
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any).openPeek === 'function'
      && (globalThis as any).eval('typeof sessions') !== 'undefined');
    await page.evaluate(() => (globalThis as any).eval(
      "localStorage.removeItem('amux_peek_head'); sessions.push({name:'zz-narrow',dir:'/tmp',running:true,status:'active',task_name:'Update brand reconfiguration plan post-launch'}); openPeek('zz-narrow')"));
    await expect(page.locator('#peek-overlay .peek-toolbar')).toBeVisible();
    // The label fills from the board; the layout is what is under test.
    await expect(page.locator('#peek-task-row')).toBeVisible();
    await page.evaluate(() => { (document.getElementById('peek-task-label') as HTMLElement).textContent = 'Update brand reconfiguration plan post-launch'; });
    const g = await page.evaluate(() => {
      const r = (s: string) => (document.querySelector(s) as HTMLElement).getBoundingClientRect();
      const head = r('#peek-overlay > .overlay-header'), task = r('#peek-task-row'), tools = r('#peek-overlay .peek-toolbar');
      const line = parseFloat(getComputedStyle(document.querySelector('#peek-task-row') as HTMLElement).lineHeight) || 20;
      const label = document.getElementById('peek-task-label') as HTMLElement;
      return { headW: head.width, headH: head.height, taskW: task.width, taskH: task.height, line, toolsL: tools.left, toolsR: tools.right, vw: innerWidth,
        fine: matchMedia('(hover: hover) and (pointer: fine)').matches, clipped: label.scrollWidth > label.clientWidth + 1 };
    });
    testInfo.annotations.push({ type: 'geometry', description: JSON.stringify(g) });
    await page.screenshot({ path: testInfo.outputPath(`narrow-${width}.png`) });
    // With a mouse the task row stretches across the band, so its width is the
    // room it was given. A touch screen keeps the stacked header, where the row
    // is centred and only as wide as its text (334px of 736 for this task at
    // 760px, iOS Safari): there the check is that the whole text shows.
    if (g.fine) expect(g.taskW, 'the task gets most of the width').toBeGreaterThan(g.headW * 0.5);
    else expect(g.clipped, 'the whole task shows').toBe(false);
    expect(g.taskH, 'the task is a line or two, not a column of letters').toBeLessThanOrEqual(g.line * 3 + 8);
    expect(g.toolsL, 'the toolbar is on screen').toBeGreaterThanOrEqual(0);
    expect(g.toolsR, 'the toolbar is on screen').toBeLessThanOrEqual(g.vw + 1);
  });
}
