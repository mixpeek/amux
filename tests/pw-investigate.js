const { chromium } = require('playwright');
const { homedir } = require('os');

(async () => {
  const ctx = await chromium.launchPersistentContext(
    `${homedir()}/.amux/playwright-auth/profile`,
    { headless: true, viewport: { width: 1440, height: 900 }, ignoreHTTPSErrors: true }
  );
  const page = await ctx.newPage();
  await page.goto('https://localhost:8822', { waitUntil: 'domcontentloaded', timeout: 10000 });
  await page.waitForTimeout(2000);

  await page.screenshot({ path: '/tmp/pw-investigate-cursor-sessions.png' });

  // Check cursor on key elements
  const cursors = await page.evaluate(() => {
    const sel = [
      'body', 'html', '.card', '.cards', '#sessions-view',
      '.tab-bar', '.tab-bar button', 'button', 'input',
      '.card-drag-handle', '[data-session]'
    ];
    return sel.map(s => {
      const el = document.querySelector(s);
      if (!el) return { sel: s, cursor: null, note: 'not found' };
      const computed = window.getComputedStyle(el).cursor;
      const inline = el.style.cursor;
      return { sel: s, computed, inline: inline || null };
    });
  });
  console.log('Cursor on Sessions tab:', JSON.stringify(cursors, null, 2));

  // Check body inline style
  const bodyStyle = await page.evaluate(() => document.body.getAttribute('style'));
  console.log('Body inline style:', bodyStyle);

  // Check if grid-mode class is applied to .cards
  const cardsInfo = await page.evaluate(() => {
    const cards = document.querySelector('.cards');
    return cards ? { className: cards.className, style: cards.getAttribute('style') } : null;
  });
  console.log('.cards element:', cardsInfo);

  // Check if any element has cursor: grab set inline (not just computed)
  const grabInline = await page.evaluate(() => {
    const all = document.querySelectorAll('*');
    const results = [];
    for (const el of all) {
      if (el.style && el.style.cursor && el.style.cursor.includes('grab')) {
        results.push({
          tag: el.tagName,
          id: el.id,
          cls: el.className?.toString()?.substring(0, 50),
          cursor: el.style.cursor
        });
      }
    }
    return results;
  });
  console.log('Elements with inline grab cursor:', JSON.stringify(grabInline, null, 2));

  // Check Sortable's effect — does it set any styles on the container?
  const sortableContainers = await page.evaluate(() => {
    const cards = document.querySelector('.cards');
    return cards ? {
      style: cards.getAttribute('style'),
      computedCursor: window.getComputedStyle(cards).cursor,
      computedUserSelect: window.getComputedStyle(cards).userSelect,
      touchAction: window.getComputedStyle(cards).touchAction,
    } : null;
  });
  console.log('Sortable .cards container styles:', JSON.stringify(sortableContainers, null, 2));

  // Now switch to Board tab and check there
  await page.click('.tab-bar button[onclick*="board"]');
  await page.waitForTimeout(1000);
  await page.screenshot({ path: '/tmp/pw-investigate-cursor-board.png' });

  const boardCursors = await page.evaluate(() => {
    const sel = ['body', '.board-col', '.board-card', '.board-drag-handle', '.board-col-body'];
    return sel.map(s => {
      const el = document.querySelector(s);
      if (!el) return { sel: s, cursor: null, note: 'not found' };
      return { sel: s, computed: window.getComputedStyle(el).cursor, inline: el.style.cursor || null };
    });
  });
  console.log('Cursor on Board tab:', JSON.stringify(boardCursors, null, 2));

  await ctx.close();
  console.log('Done.');
})();
