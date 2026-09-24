// Click-only end-to-end run of worker types in the real dashboard (ACW-11).
// Every ACTION is a click, tap or keystroke; page.evaluate only READS state.
//
// Needs a running amux server with at least these workers: helper-chat (chat,
// running), code-lane (coding, STOPPED; it is switched to chat and back).
// Run it against an ISOLATED server, never the live one: it creates workers
// and spends real model turns.
//
//   AMUX_HOME=$tmp/home TMUX_TMPDIR=$(mktemp -d /tmp/amuxt.XXXX) AMUX_RS_PORT=18824 //     env -u TMUX -u TMUX_PANE -u AMUX_SESSION -u AMUX_URL amux-server
//
// Unset TMUX: with it set, tmux ignores TMUX_TMPDIR and the test server
// reaches the real fleet's panes (2026-09-24, see frustrations.md).
//
//   AMUX_E2E_URL=https://localhost:18824 AMUX_E2E_TOKEN=$(cat $tmp/home/auth_token) //     AMUX_E2E_OUT=/tmp/wt-e2e node e2e/worker-types-click.cjs
const { chromium, devices } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const fs = require('fs');
const OUT = process.env.AMUX_E2E_OUT || '/tmp/worker-types-e2e';
const URL = (process.env.AMUX_E2E_URL || 'https://localhost:18824') + '/?_token=' + (process.env.AMUX_E2E_TOKEN || '');
const results = [];
const CHAT = 'e2e-chat-' + Date.now().toString(36);
const ok = (step, pass, detail) => { results.push({ step, pass, detail }); console.log((pass ? 'PASS ' : 'FAIL ') + step + (detail ? ' :: ' + detail : '')); };
const shot = (page, n) => page.screenshot({ path: `${OUT}/${n}.png` });
const visibleTexts = (page, sel) => page.$$eval(sel, els => els.filter(e => e.offsetParent !== null).map(e => e.textContent.trim()));
const bubbles = page => page.$$eval('#peek-body .chat-msg', els => els.map(e => ({ cls: e.className, text: e.querySelector('.chat-bubble').textContent.trim() })));

async function openWorker(page, name, touch) {
  const title = page.locator(`.card[data-session="${name}"] .card-header, .card[data-session="${name}"] [onclick*="headerTap"]`).first();
  if (touch) {
    // Tap the NAME: the middle of the header is the status button on a phone,
    // which opens the status dialog and swallows the second tap.
    const nameEl = page.locator(`.card[data-session="${name}"] .card-header`).getByText(name, { exact: true }).first();
    await nameEl.tap(); await page.waitForTimeout(120); await nameEl.tap();
  }
  else {
    // The worker menu's open entry: "Open chat" or "Peek terminal" by renderer.
    await page.locator(`.card[data-session="${name}"] [onclick*="toggleMenu"]`).click();
    await page.locator('.card-menu-item', { hasText: /Open chat|Peek terminal/ }).locator('visible=true').first().click();
  }
  await page.waitForSelector('#peek-overlay.active', { timeout: 10000 });
}
async function closeWorker(page) {
  await page.locator('#peek-overlay [onclick="closePeek()"]').locator('visible=true').first().click();
  await page.waitForTimeout(400);
}
async function waitAssistantCount(page, n, timeoutMs) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    const b = await bubbles(page);
    const done = b.filter(x => x.cls.includes('chat-assistant') && !x.cls.includes('is-streaming'));
    if (done.length >= n && !b.some(x => x.cls.includes('is-streaming'))) return done;
    await page.waitForTimeout(300);
  }
  return null;
}
// Sample the streaming bubble; return how many DISTINCT non-empty lengths we saw.
async function watchStream(page, timeoutMs, shotName) {
  const lens = new Set(); let shotTaken = false; const t0 = Date.now(); let sawBubble = false;
  while (Date.now() - t0 < timeoutMs) {
    const t = await page.evaluate(() => { const b = document.querySelector('#peek-body .chat-msg.is-streaming .chat-bubble'); return b ? b.textContent : null; });
    if (t !== null) sawBubble = true;
    if (t && t.trim()) { lens.add(t.length); if (!shotTaken && lens.size >= 2) { await shot(page, shotName); shotTaken = true; } }
    if (sawBubble && t === null) break;
    await page.waitForTimeout(150);
  }
  return { distinct: lens.size, sawBubble };
}

(async () => {
  fs.mkdirSync(OUT, { recursive: true });
  const browser = await chromium.launch();
  const errors = [];

  // ================= DESKTOP =================
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 1280, height: 860 } });
  // A returning user: the first-run tour runs view-switching actions on a
  // timer that close the worker window mid-test in a fresh profile.
  await ctx.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  const page = await ctx.newPage();
  page.on('pageerror', e => errors.push(e.message));
  page.on('dialog', d => { console.log('dialog: ' + d.message().slice(0, 80)); d.accept(); });
  await page.goto(URL);
  await page.waitForSelector('.card[data-session="helper-chat"]', { timeout: 30000 });
  await page.waitForTimeout(1200);
  const dskip = page.getByText('Skip', { exact: true });
  if (await dskip.isVisible().catch(() => false)) { await dskip.click(); await page.waitForTimeout(600); }

  // 1. Create a chat worker through the + menu
  await page.locator('#add-btn').click();
  await page.getByText('New worker', { exact: false }).first().click();
  await page.waitForSelector('#create-overlay.active');
  await page.locator('#create-name').fill(CHAT);
  const typeBtns = await visibleTexts(page, '#create-type-row button');
  ok('create modal offers types from the registry', JSON.stringify(typeBtns) === '["Coding","Chat"]', JSON.stringify(typeBtns));
  await page.locator('#create-type-row button', { hasText: 'Chat' }).click();
  const disabled = await page.$$eval('#create-overlay .provider-btn', els => els.filter(e => e.disabled).map(e => e.textContent.trim()));
  ok('chat disables providers its adapter cannot drive', disabled.includes('Gemini') && disabled.includes('Ollama') && !disabled.includes('Claude Code'), JSON.stringify(disabled));
  ok('chat hides branch/worktree options', !(await page.locator('#create-branch-enabled').isVisible()));
  await page.locator('#create-dir').fill('');
  await page.locator('#create-prompt').fill('Reply with exactly: hello from e2e');
  await shot(page, '01-create-chat');
  await page.locator('#create-overlay button', { hasText: 'Create' }).click();
  await page.waitForSelector(`.card[data-session="${CHAT}"]`, { timeout: 20000 });
  await page.waitForTimeout(800);
  const badge = await visibleTexts(page, `.card[data-session="${CHAT}"] .badge.worker-type`);
  ok('new card shows the chat badge', badge.join() === 'chat', JSON.stringify(badge));
  await shot(page, '02-card-list');

  // 2. Open it by double-clicking the card header
  await openWorker(page, CHAT, false);
  ok('worker window opens on the Chat tab', (await page.locator('#peek-tab-terminal .tab-lbl').textContent()) === 'Chat');
  const first = await waitAssistantCount(page, 1, 90000);
  ok('first prompt answered and rendered', !!first && /hello from e2e/i.test(first[0].text), first && first[0].text);
  const chips = await visibleTexts(page, '#peek-chips .chip');
  ok('no terminal keystroke chips in the chat window', !chips.some(c => /^(Enter|Esc|Space|Ctrl C|↑ Up|↓ Down)$/.test(c)), JSON.stringify(chips));
  ok('Worktree tab hidden for chat', !(await page.locator('#peek-tab-git').isVisible()));
  await shot(page, '03-chat-first-reply');

  // 3. Send by clicking Send and WATCH the reply stream in
  await page.locator('#peek-cmd-input').click();
  await page.keyboard.type('Count from 1 to 60, separated by spaces, nothing else.');
  await page.locator('#peek-overlay .send-split-main').click();
  const s1 = await watchStream(page, 90000, '04-streaming-midway');
  ok('reply visibly streams (bubble grew across samples)', s1.sawBubble && s1.distinct >= 3, JSON.stringify(s1));
  const second = await waitAssistantCount(page, 2, 60000);
  ok('streamed reply finalized', !!second && /58 59 60/.test(second[1].text), second && second[1].text.slice(-40));
  const userShown = (await bubbles(page)).filter(b => b.cls.includes('chat-user')).map(b => b.text);
  ok('user bubble shows the text without the send-time stamp', userShown.some(t => t.startsWith('Count from 1 to 60')), JSON.stringify(userShown.slice(-1)));
  await shot(page, '05-streamed-reply');

  // 4. Queue: two sends back to back
  await page.locator('#peek-cmd-input').click();
  await page.keyboard.type('Reply with exactly: first');
  await page.locator('#peek-overlay .send-split-main').click();
  await page.waitForTimeout(700);
  await page.locator('#peek-cmd-input').click();
  await page.keyboard.type('Reply with exactly: second');
  await page.locator('#peek-overlay .send-split-main').click();
  let sawQueued = false;
  for (let i = 0; i < 40 && !sawQueued; i++) { sawQueued = await page.locator('.chat-queued').isVisible().catch(() => false); if (!sawQueued) await page.waitForTimeout(250); }
  const four = await waitAssistantCount(page, 4, 120000);
  const lastTwo = four ? four.slice(-2).map(b => b.text) : [];
  ok('back-to-back sends queue and answer in order', !!four && /first/i.test(lastTwo[0]) && /second/i.test(lastTwo[1]), JSON.stringify(lastTwo));
  ok('queued indicator shows while the second message waits', sawQueued);

  // 5. Shared tabs work for a chat worker
  await page.locator('#peek-tab-messages').click();
  await page.waitForTimeout(1500);
  const msgText = await page.locator('#peek-messages-panel').textContent();
  ok('Messages tab lists the chat messages (shared ledger)', /Count from 1 to 60/.test(msgText));
  await shot(page, '06-messages-tab');
  await page.locator('#peek-tab-scope').click();
  await page.waitForTimeout(1500);
  const typeRow = await page.locator('#peek-scope-body').textContent();
  ok('Configurations shows the Worker type row', /Worker type/.test(typeRow));
  await shot(page, '07-configurations');
  await page.locator('#peek-tab-terminal').click();
  await page.waitForTimeout(500);
  await closeWorker(page);

  // 6. The card's own composer
  await page.locator(`.card[data-session="${CHAT}"]`).click();
  await page.waitForTimeout(600);
  const cardChips = await visibleTexts(page, `.card[data-session="${CHAT}"] .chip`);
  ok('card composer has no keystroke chips for chat', !cardChips.some(c => /^(Enter|Esc|Space|Ctrl C)$/.test(c)), JSON.stringify(cardChips));
  await page.locator(`#input-${CHAT}`).fill('Reply with exactly: from the card');
  await page.locator(`.card[data-session="${CHAT}"] button`, { hasText: 'Send' }).first().click();
  await page.waitForTimeout(1000);
  await openWorker(page, CHAT, false);
  const five = await waitAssistantCount(page, 5, 90000);
  ok('card-composer message answered in the chat', !!five && /from the card/i.test(five[4].text), five && five[4].text);
  await closeWorker(page);

  // 7. Worker menu: Open chat label, no Clear scrollback, Stop, Start
  await page.locator(`.card[data-session="${CHAT}"] [onclick*="toggleMenu"]`).click();
  await page.waitForTimeout(400);
  const menu = await visibleTexts(page, '.card-menu-item, .menu-item, [role="menuitem"]');
  ok('menu says "Open chat", hides "Clear scrollback"', menu.some(t => /Open chat/.test(t)) && !menu.some(t => /Clear scrollback/.test(t)), JSON.stringify(menu.filter(t => /chat|terminal|scrollback|Stop/i.test(t))));
  await page.locator('.card-menu-item', { hasText: /Stop$/ }).locator('visible=true').first().click();
  await page.waitForSelector(`.card[data-session="${CHAT}"] button:has-text("Start")`, { timeout: 20000 }).catch(() => {});
  const stopped = await page.locator(`.card[data-session="${CHAT}"] button:has-text("Start")`).isVisible();
  ok('Stop from the menu stops it (Start button appears)', stopped);
  await shot(page, '08-stopped');
  await page.locator(`.card[data-session="${CHAT}"] button:has-text("Start")`).click();
  let running = false;
  for (let i = 0; i < 40 && !running; i++) { await page.waitForTimeout(500); running = !(await page.locator(`.card[data-session="${CHAT}"] button:has-text("Start")`).isVisible()); }
  ok('Start brings it back', running);

  // 8. Switch a coding worker to chat and back from Configurations
  await openWorker(page, 'code-lane', false);
  ok('coding worker opens on the Terminal tab', (await page.locator('#peek-tab-terminal .tab-lbl').textContent()) === 'Terminal');
  const codeChips = await visibleTexts(page, '#peek-chips .chip');
  ok('coding worker keeps keystroke chips', codeChips.includes('Enter') && codeChips.includes('Esc'), JSON.stringify(codeChips));
  await page.locator('#peek-tab-scope').click();
  await page.waitForTimeout(1500);
  await page.locator('#peek-scope-body button', { hasText: /^Chat$/ }).first().click();
  await page.waitForTimeout(2500);
  ok('type switched to Chat from Configurations', (await page.locator('#peek-tab-terminal .tab-lbl').textContent()) === 'Chat');
  await shot(page, '09-switched-to-chat');
  await page.locator('#peek-tab-scope').click();
  await page.waitForTimeout(1200);
  await page.locator('#peek-scope-body button', { hasText: /^Coding$/ }).first().click();
  await page.waitForTimeout(2500);
  ok('type switched back to Coding', (await page.locator('#peek-tab-terminal .tab-lbl').textContent()) === 'Terminal');
  await closeWorker(page);
  const codeBadge = await visibleTexts(page, '.card[data-session="code-lane"] .badge.worker-type');
  ok('coding card carries no type badge again', codeBadge.length === 0, JSON.stringify(codeBadge));

  // 9. Reload with the chat open: the window is restored with its history
  await openWorker(page, CHAT, false);
  await page.waitForTimeout(1500);
  const before = (await bubbles(page)).filter(b => b.cls.includes('chat-assistant')).length;
  await page.reload();
  await page.waitForSelector('#peek-overlay.active', { timeout: 30000 });
  let afterReload = 0;
  for (let i = 0; i < 30 && afterReload < before; i++) { await page.waitForTimeout(300); afterReload = (await bubbles(page)).filter(b => b.cls.includes('chat-assistant')).length; }
  const restoredTo = await page.evaluate(() => peekSession);
  ok('reload restores the chat with full history', restoredTo === CHAT && afterReload === before && before >= 5, JSON.stringify({ restoredTo, before, afterReload }));
  await shot(page, '09b-after-reload');
  await closeWorker(page);

  // ================= PHONE (iPhone 13, touch) =================
  const mctx = await browser.newContext({ ...devices['iPhone 13'], ignoreHTTPSErrors: true });
  await mctx.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  const m = await mctx.newPage();
  m.on('pageerror', e => errors.push('mobile: ' + e.message));
  await m.goto(URL);
  await m.waitForSelector(`.card[data-session="${CHAT}"]`, { timeout: 30000 });
  await m.waitForTimeout(1500);
  const skip = m.getByText('Skip', { exact: true });
  if (await skip.isVisible().catch(() => false)) { await skip.tap(); await m.waitForTimeout(600); }
  await shot(m, '10-phone-list');
  console.log('phone state:', JSON.stringify(await m.evaluate(() => ({ active: document.getElementById('peek-overlay').classList.contains('active'), peekSession, cards: [...document.querySelectorAll('.card[data-session]')].map(c => c.dataset.session + ':' + (c.offsetParent !== null)) }))));
  await openWorker(m, CHAT, true);
  await m.waitForTimeout(2000);
  ok('phone: chat window opens by tapping', (await m.locator('#peek-tab-terminal .tab-lbl').textContent()) === 'Chat');
  await m.locator('#peek-cmd-input').tap();
  await m.keyboard.type('Name three colors, comma separated.');
  await m.locator('#peek-overlay .send-split-main').tap();
  const ms = await watchStream(m, 90000, '11-phone-streaming');
  const mDone = await waitAssistantCount(m, 6, 60000);
  ok('phone: send by tap, reply streams and lands', !!mDone && ms.sawBubble, JSON.stringify({ ms, last: mDone && mDone[5].text }));
  const overflow = await m.$eval('#peek-body', e => e.scrollWidth - e.clientWidth);
  ok('phone: no horizontal overflow in the chat', overflow <= 0, 'overflow px ' + overflow);
  const sendBox = await m.locator('#peek-overlay .send-split-main').boundingBox();
  ok('phone: Send tap target >= 44px tall', sendBox && sendBox.height >= 44, JSON.stringify(sendBox));
  await shot(m, '12-phone-chat');

  ok('no page errors', errors.length === 0, JSON.stringify(errors.slice(0, 5)));
  fs.writeFileSync(OUT + '/results.json', JSON.stringify(results, null, 2));
  const failed = results.filter(r => !r.pass).length;
  console.log(`\n${results.length - failed}/${results.length} passed`);
  await browser.close();
})().catch(e => { console.error('CRASH', e); fs.writeFileSync(OUT + '/results.json', JSON.stringify(results, null, 2)); process.exit(1); });
