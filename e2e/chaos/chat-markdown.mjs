#!/usr/bin/env node
// END-TO-END: the chat view renders a STREAMED reply as sanitized markdown.
// Real dashboard, private server, the shipped _chatRender, fed token by token.
// Screenshots are written to the artifacts dir (mid-stream and final).
// Usage: AMUX_CHAOS_BINARY=<amux-server> node e2e/chaos/chat-markdown.mjs
import path from 'node:path';
import { chromium } from 'playwright';
import { startAmux, waitFor } from './harness.mjs';

const REPLY = [
  '## Signup flow check\n\n',
  'Here is what I found for **isabella.potes@indriver.com**:\n\n',
  '- Namespace created\n- Research email: *not sent*\n- See the [playbook docs](https://amux.io/docs) for the flow\n\n',
  '| step | status |\n|---|---|\n| namespace | ok |\n| email | missing |\n\n',
  '```python\ndef send_research_email(user):\n    return queue.enqueue(user.id)\n```\n\n',
  'Screenshot of the dashboard:\n\n![amux icon](/icon-192.png)\n\n',
  'Tried <img src=x onerror="window.__xss=1"> and [bad](javascript:window.__xss=2).',
].join('');

const checks = [];
const check = (name, ok, detail) => checks.push({ name, ok: !!ok, detail });
const amux = await startAmux({ binary: process.env.AMUX_CHAOS_BINARY });
let browser;
try {
  await amux.req('POST', '/api/sessions', { name: 'chatty', dir: amux.root, start: false });
  browser = await chromium.launch();
  const page = await (await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 390, height: 844 } })).newPage();
  const errors = []; page.on('pageerror', e => errors.push(String(e)));
  await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
  await page.goto(amux.base + '/', { waitUntil: 'domcontentloaded' });
  await page.waitForFunction(() => typeof _chatRender === 'function' && typeof marked !== 'undefined' && typeof remend === 'function', null, { timeout: 20000 });
  check('marked, DOMPurify and remend load from amux (no CDN)', await page.evaluate(() =>
    [...document.scripts].some(s => s.src.endsWith('/vendor/md.js')) && ![...document.scripts].some(s => /jsdelivr\.net\/npm\/(marked|dompurify)/.test(s.src))));
  await page.evaluate(() => { openPeek('chatty'); });
  await page.waitForTimeout(800);
  // Put the view in chat mode with no live stream, then feed tokens.
  await page.evaluate(() => { try { _chatUnmount(); } catch (e) {} _chat.name = 'chatty'; _chat.messages = [{ role: 'user', text: 'did the signup email go out?', ts: Date.now() / 1000 }]; _chat.streaming = { turn_id: 't1', text: '' }; document.getElementById('peek-body').classList.add('peek-chat'); });
  const feed = async (upto) => page.evaluate(t => { _chat.streaming.text = t; _chatRender(); }, REPLY.slice(0, upto));
  const bubble = () => page.evaluate(() => {
    const b = [...document.querySelectorAll('.chat-msg.is-streaming .chat-bubble, .chat-msg.chat-assistant .chat-bubble')].pop();
    const r = b.getBoundingClientRect();
    return { text: b.innerText, html: b.innerHTML, width: r.width, right: r.right, scrollW: document.getElementById('peek-body').scrollWidth, clientW: document.getElementById('peek-body').clientWidth };
  });
  // Mid-link: cut inside "(https://amux.io/do".
  const midLink = REPLY.indexOf('https://amux.io/docs') + 12;
  await feed(midLink);
  let b = await bubble();
  check('mid-link: no raw ]( markup shown', !b.text.includes('](') && !b.text.includes('[playbook'), b.text.slice(-120));
  check('mid-link: no clickable half link', !/href="https:\/\/amux\.io\/do"/.test(b.html), b.html.slice(-200));
  await page.screenshot({ path: path.join(amux.root, 'chat-1-mid-link.png') });
  // Mid-fence: cut inside the code block.
  const midFence = REPLY.indexOf('return queue');
  await feed(midFence);
  b = await bubble();
  check('mid-fence: no raw ``` shown, code already rendered as code', !b.text.includes('```') && /<pre><code/.test(b.html), b.text.slice(-120));
  check('mid-fence: layout holds (no horizontal overflow)', b.scrollW <= b.clientW + 1, b);
  await page.screenshot({ path: path.join(amux.root, 'chat-2-mid-fence.png') });
  // Stream the rest one chunk at a time, rendering every step.
  for (let i = midFence; i <= REPLY.length; i += 7) await feed(i);
  await feed(REPLY.length);
  await page.waitForTimeout(600);
  b = await bubble();
  check('final: heading, list, table, code', /<h2/.test(b.html) && /<li>/.test(b.html) && /<table/.test(b.html) && /<pre><code/.test(b.html));
  check('final: link opens in a new tab with rel=noopener', /<a href="https:\/\/amux\.io\/docs" target="_blank" rel="noopener"/.test(b.html), b.html.match(/<a [^>]*amux\.io[^>]*>/)?.[0]);
  const img = await page.evaluate(() => { const i = [...document.querySelectorAll('.chat-bubble img')].find(x => x.src.endsWith('/icon-192.png')); if (!i) return null; const r = i.getBoundingClientRect(), br = i.closest('.chat-bubble').getBoundingClientRect(); return { loaded: i.complete && i.naturalWidth > 0, fits: r.width <= br.width + 1, w: r.width, bw: br.width }; });
  check('final: image renders inline and fits the bubble', img && img.loaded && img.fits, img);
  check('sanitized: onerror stripped, javascript: link neutered, nothing executed',
    !/onerror/i.test(b.html) && !/javascript:/i.test(b.html) && !(await page.evaluate(() => window.__xss)), b.html.slice(-300));
  await page.screenshot({ path: path.join(amux.root, 'chat-3-final.png'), fullPage: false });
  // Also the finished message path (not streaming).
  await page.evaluate(t => { _chat.streaming = null; _chat.messages.push({ role: 'assistant', text: t, ts: Date.now() / 1000 }); _chatRender(); }, REPLY);
  b = await bubble();
  check('finished message renders the same markdown', /<table/.test(b.html) && /amux\.io\/docs/.test(b.html));
  check('no uncaught page errors', errors.length === 0, errors.slice(0, 3));
} catch (e) {
  check('harness ran to completion', false, String(e && e.stack || e));
} finally {
  if (browser) await browser.close().catch(() => {});
  await amux.stop();
}
const failed = checks.filter(c => !c.ok);
console.log(JSON.stringify({ measured: checks.length > 0, n_considered: checks.length, failed: failed.length, artifacts: amux.root, checks }, null, 1));
console.log('VERDICT:', checks.length && !failed.length ? 'PASS' : 'FAIL');
process.exit(checks.length && !failed.length ? 0 : 1);
