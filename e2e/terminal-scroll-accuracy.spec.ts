import { test, expect } from './fixtures';

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto('/');
  await page.waitForFunction(() => typeof (window as any)._peekHtml === 'function');
  await page.evaluate(() => {
    eval("peekSession='scroll-worker'; _peekMsgRowsFor=peekSession; _peekMsgRows=[]; _peekMsgNavKind='all'; _peekMsgIndex=-1; _peekHistoryHTML=''; _peekEarlier={chunks:[],loadedKb:0,done:true,hidden:true,loading:false};");
    { // Open it the way openPeek does: since 90bedca6 the overlay starts hidden, inert and aria-hidden.
      const ov = document.getElementById('peek-overlay')!; ov.hidden = false; ov.inert = false;
      ov.setAttribute('aria-hidden', 'false'); ov.classList.add('active'); }
    (window as any)._stopPeekPoll();
  });
});

async function render(page: import('@playwright/test').Page, raw: string) {
  await page.evaluate(raw => {
    eval('lastPeekHTML=_peekHtml(' + JSON.stringify(raw) + '); _lastLiveHTML=lastPeekHTML;');
    (window as any).applyPeekSearch(false, false);
    document.getElementById('peek-body')!.scrollTop = 0;
  }, raw);
}

async function readable(page: import('@playwright/test').Page, selector: string) {
  const location = await page.locator(selector).first().evaluate(el => {
    const body = document.getElementById('peek-body')!;
    const target = el.getBoundingClientRect(), bounds = body.getBoundingClientRect();
    // The prompt's decorative left border can extend outside the padding;
    // measure its actual text when deciding whether the words are readable.
    const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    let node: Node | null;
    do { node = walker.nextNode(); } while (node && !node.textContent?.trim());
    const range = document.createRange();
    if (node) range.selectNodeContents(node);
    const glyph = node ? range.getClientRects()[0] : target;

    // Only the subagent navigator floats over the output now.
    const controls = [...document.querySelectorAll('.peek-output-controls .peek-agent-nav')]
      .filter(e => e.getClientRects().length).map(e => e.getBoundingClientRect().bottom);
    return { top: target.top, bottom: bounds.bottom, safe: Math.max(bounds.top, ...controls),
      left: glyph.left, bodyLeft: bounds.left, bodyRight: bounds.right,
      horizontal: glyph.left >= bounds.left - 1 && glyph.left < bounds.right,
      pageTop: document.scrollingElement!.scrollTop };
  });
  expect(location.top).toBeGreaterThanOrEqual(location.safe);
  expect(location.top).toBeLessThan(location.bottom);
  expect(location.horizontal, JSON.stringify(location)).toBe(true);
  expect(location.pageTop).toBe(0);
}

for (const zoom of [1, 0.8, 1.25]) {
  test(`message order and readable landings at ${zoom * 100}% zoom`, async ({ page }) => {
    const beacons: any[] = [];
    await page.route('**/api/client-debug', async route => {
      beacons.push(route.request().postDataJSON());
      await route.fulfill({ json: { ok: true } });
    });
    await page.evaluate(zoom => { document.body.style.zoom = String(zoom); }, zoom);
    const labels = ['first α 🧭 & <tag>', '[amux-origin:producer] supplied /tmp/report.txt', '[Scheduled] verify the result', 'last short message'];
    const raw = 'before\n'.repeat(50) + '› ' + labels[0] + '\n  ' + 'A wrapped paragraph. '.repeat(900)
      + '\nAssistant reply\n' + 'between\n'.repeat(30) + '❯ ' + labels[1]
      + '\nAssistant reply\n' + 'between\n'.repeat(30) + '❯ ' + labels[2]
      + '\nAssistant reply\n' + 'between\n'.repeat(30) + '› ' + labels[3];
    await render(page, raw);
    for (const index of [0, 1, 2, 3, 0]) {
      await page.getByRole('button', { name: 'Next message', exact: true }).click();
      await expect(page.locator('.peek-msg-current')).toContainText(labels[index]);
      await readable(page, '.peek-msg-current');
    }
    await page.getByRole('button', { name: 'Previous message', exact: true }).click();
    await expect(page.locator('.peek-msg-current')).toContainText(labels[3]);
    await readable(page, '.peek-msg-current');
    await expect.poll(() => beacons.filter(b => b.kind === 'peek-message-nav' && b.verdict === 'landed').length).toBe(6);
    for (const b of beacons.filter(b => b.kind === 'peek-message-nav')) {
      expect(b.measured).toBe(true);
      expect(b.target_visible).toBe(true);
      expect(Math.abs(b.scroll_error_px)).toBeLessThan(2);
      expect(b.zoom).toBeCloseTo(zoom, 2);
      // Controls now occupy a sibling row. The landing still clears them
      // (readable() above) with a 12px reading margin inside the scroller.
      expect(b.desired_inset).toBeGreaterThanOrEqual(11.95);
    }
  });
}

for (const sample of [
  { text: 'red & blue <tag>', query: 'red & blue <tag>' },
  { text: 'multi\x1b[31mcolor\x1b[0m phrase', query: 'multicolor phrase' },
  { text: '/tmp/report-α.md has 🧭 notes', query: '/tmp/report-α.md' },
]) {
  test(`Find navigates rendered text: ${sample.query}`, async ({ page }) => {
    await render(page, 'before\n'.repeat(45) + sample.text + '\n' + 'between\n'.repeat(50) + sample.text + '\n' + 'after\n'.repeat(50));
    await page.getByRole('button', { name: 'Find in terminal', exact: true }).click();
    await page.getByRole('searchbox', { name: 'Find in terminal', exact: true }).fill(sample.query);
    await expect(page.locator('#peek-msg-count')).toHaveText('1/2');
    await readable(page, '.peek-highlight.current');
    expect(await page.locator('.peek-highlight.current').allTextContents()).not.toHaveLength(0);
    expect((await page.locator('.peek-highlight.current').allTextContents()).join('')).toBe(sample.query);
    await page.getByRole('button', { name: 'Next message', exact: true }).click();
    await expect(page.locator('#peek-msg-count')).toHaveText('2/2');
    await readable(page, '.peek-highlight.current');
    await page.locator('#peek-search').press('Enter');
    await expect(page.locator('#peek-msg-count')).toHaveText('1/2');
    await page.locator('#peek-search').press('Shift+Enter');
    await expect(page.locator('#peek-msg-count')).toHaveText('2/2');
  });
}

test('Find reveals text inside a wide terminal table', async ({ page }) => {
  await render(page, 'before\n'.repeat(40) + '│ ' + 'wide column '.repeat(100) + 'far-token │\n' + 'after\n'.repeat(40));
  await page.getByRole('button', { name: 'Find in terminal', exact: true }).click();
  await page.getByRole('searchbox', { name: 'Find in terminal', exact: true }).fill('far-token');
  await expect(page.locator('#peek-msg-count')).toHaveText('1/1');
  await readable(page, '.peek-highlight.current');
  expect(await page.locator('.peek-box').evaluate(el => el.scrollLeft)).toBeGreaterThan(0);
});

for (const mode of ['message', 'search']) for (const tail of [50, 0]) {
  test(`a working worker refresh keeps the selected ${mode} and its position with ${tail} trailing lines`, async ({ page }) => {
    // A stable, empty subagent list: the fixture worker has none, and the
    // refresh's failing fetch showed the navigator mid-test, which moves the
    // output on its own (that case has its own test below).
    await page.route('**/api/sessions/scroll-worker/subagents', route => route.fulfill({ json: { session: 'scroll-worker', subagents: [] } }));
    const raw = 'before\n'.repeat(45) + '› selected original text\nAssistant\n' + 'after\n'.repeat(tail);
    await render(page, raw);
    if (mode === 'search') {
      await page.getByRole('button', { name: 'Find in terminal', exact: true }).click();
      await page.getByRole('searchbox', { name: 'Find in terminal', exact: true }).fill('selected original text');
    } else await page.getByRole('button', { name: 'Next message', exact: true }).click();
    const selector = mode === 'search' ? '.peek-highlight.current' : '.peek-msg-current';
    // Let the asynchronous browser scroll event run, including bottom-clamped
    // jumps. Calling refresh synchronously hid the second unlock path.
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    const target = await page.locator(selector).elementHandle();
    const top = await page.locator('#peek-body').evaluate(el => el.scrollTop);
    await page.route('**/api/sessions/scroll-worker/peek?*', route => route.fulfill({ json: {
      output: 'new output\n'.repeat(100) + raw + '\nlatest output', history: '',
    } }));
    await page.evaluate(() => (window as any).refreshPeek());
    expect(await target!.evaluate(el => el.isConnected)).toBe(true);
    expect(await page.locator('#peek-body').evaluate(el => el.scrollTop)).toBe(top);
    await readable(page, selector);
  });
}


// The subagent navigator appearing adds top padding to the output; a selected
// hit near the bottom must stay in view (680b0ecd3's CI red on iPhone).
for (const mode of ['message', 'search']) {
  test(`the ${mode} stays in view when the subagent navigator appears`, async ({ page }) => {
    await render(page, 'before\n'.repeat(45) + '› selected original text\nAssistant\n');
    if (mode === 'search') {
      await page.getByRole('button', { name: 'Find in terminal', exact: true }).click();
      await page.getByRole('searchbox', { name: 'Find in terminal', exact: true }).fill('selected original text');
    } else await page.getByRole('button', { name: 'Next message', exact: true }).click();
    const selector = mode === 'search' ? '.peek-highlight.current' : '.peek-msg-current';
    await readable(page, selector);
    await page.evaluate(() => { (window as any).eval("_peekAgents.error = 'unavailable'; _peekAgentsPaint();"); });
    await expect(page.locator('#peek-agent-nav')).toBeVisible();
    await readable(page, selector);
  });
}

test('saved Codex input hints stay out of navigation while recorded messages remain', async ({ page }) => {
  const raw = '› Ask Codex to do anything\n\n  gpt-6-astra xhigh · ~/project\n'
    + 'later assistant output\n'.repeat(8) + '› actual submitted request\nAssistant reply\n';
  await render(page, raw);
  await expect(page.locator('#peek-body .peek-prompt')).toHaveCount(1);
  await expect(page.locator('#peek-body .peek-composer-hint')).toHaveCount(1);
  await expect(page.locator('#peek-body .peek-prompt')).toContainText('actual submitted request');
  await page.evaluate(() => {
    eval("_peekMsgRows=[{session:peekSession,type:'direct',text:'Ask Codex to do anything'}];");
  });
  await render(page, raw);
  await expect(page.locator('#peek-body .peek-prompt-human')).toContainText('Ask Codex to do anything');
});
