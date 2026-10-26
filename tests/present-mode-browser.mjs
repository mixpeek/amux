// Can a deck in the file viewer be presented full screen? (Ethan, 2026-10-25:
// "amux file view of ppt (or pdf) can be used to present full screen to an
// audience".)
//
// Drives the SHIPPED present mode (_filePresent) over a generated 3-page PDF:
// each page is one solid colour, so a wrong page, a dashboard pixel leaking
// through, or a resampled slide shows up as a colour that should not be there.
// A .pptx with its PDF export beside it must offer Present; one without must
// say why instead of offering a button that cannot work.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import {chromium} from 'playwright';

const root = path.resolve(import.meta.dirname, '..');
const base = process.env.AMUX_TEST_URL || 'https://localhost:8824';
const source = await fs.readFile(path.join(root, 'crates/amux-dashboard/static/app.js'), 'utf8');
const css = await fs.readFile(path.join(root, 'crates/amux-dashboard/static/app.css'), 'utf8');
const html = await fs.readFile(path.join(root, 'crates/amux-dashboard/static/index.html'), 'utf8');

// 16:9 pages (720 x 405 pt), solid fills, a link on the last page.
const COLORS = [[230, 40, 40], [40, 160, 60], [40, 70, 220]];
function fixturePdf() {
  const objs = [];
  const kids = COLORS.map((_, i) => 3 + i * 2);
  objs.push('<< /Type /Catalog /Pages 2 0 R >>');
  objs.push(`<< /Type /Pages /Kids [${kids.map(k => k + ' 0 R').join(' ')}] /Count ${COLORS.length} >>`);
  COLORS.forEach(([r, g, b], i) => {
    const content = `${(r / 255).toFixed(4)} ${(g / 255).toFixed(4)} ${(b / 255).toFixed(4)} rg 0 0 720 405 re f`;
    const annots = i === COLORS.length - 1
      ? ' /Annots [<< /Type /Annot /Subtype /Link /Rect [500 300 700 380] /Border [0 0 0] /A << /S /URI /URI (https://example.com/deck-link) >> >>]'
      : '';
    objs.push(`<< /Type /Page /Parent 2 0 R /MediaBox [0 0 720 405] /Contents ${4 + i * 2} 0 R${annots} >>`);
    objs.push(`<< /Length ${content.length} >>\nstream\n${content}\nendstream`);
  });
  let out = '%PDF-1.4\n';
  const offs = [];
  objs.forEach((o, i) => { offs.push(out.length); out += `${i + 1} 0 obj\n${o}\nendobj\n`; });
  const xref = out.length;
  out += `xref\n0 ${objs.length + 1}\n0000000000 65535 f \n` + offs.map(o => String(o).padStart(10, '0') + ' 00000 n \n').join('');
  out += `trailer\n<< /Size ${objs.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return Buffer.from(out, 'latin1');
}
const pdf = fixturePdf();
const FILES = {
  '/fixture/deck.pdf': {path: '/fixture/deck.pdf', is_pdf: true, size: pdf.length, raw_url: '/api/file/raw?path=%2Ffixture%2Fdeck.pdf'},
  '/fixture/deck.pptx': {path: '/fixture/deck.pptx', is_binary: true, size: 1234, ext: '.pptx'},
  '/fixture/lonely.pptx': {path: '/fixture/lonely.pptx', is_binary: true, size: 1234, ext: '.pptx'},
};

const browser = await chromium.launch({headless: true});
try {
  for (const [width, height, dpr] of [[1280, 720, 1], [1440, 900, 2]]) {
    const context = await browser.newContext({ignoreHTTPSErrors: true, serviceWorkers: 'block',
      viewport: {width, height}, deviceScaleFactor: dpr});
    await context.addInitScript(() => {
      localStorage.setItem('amux_walkthrough_done', '1');
      window.EventSource = class {
        static OPEN = 1; static CLOSED = 2; readyState = 1;
        close() {this.readyState = 2;} addEventListener() {} removeEventListener() {}
      };
    });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', e => errors.push(String(e)));
    await page.route('**/app.js*', r => r.fulfill({contentType: 'text/javascript', body: source}));
    await page.route('**/app.css*', r => r.fulfill({contentType: 'text/css', body: css}));
    // The served shell carries the server's asset stamps; swap in this tree's
    // file-overlay markup so the Present button under test is the one in it.
    await page.route(u => u.pathname === '/' || u.pathname === '/index.html', async r => {
      const resp = await r.fetch();
      let body = await resp.text();
      const [a, b] = [html.indexOf('<div id="file-overlay"'), html.indexOf('<div id="file-body"')];
      const [a2, b2] = [body.indexOf('<div id="file-overlay"'), body.indexOf('<div id="file-body"')];
      if (a > 0 && a2 > 0) body = body.slice(0, a2) + html.slice(a, b) + body.slice(b2);
      return r.fulfill({response: resp, body});
    });
    await page.route('**/api/**', r => {
      const u = new URL(r.request().url());
      if (r.request().method() !== 'GET') return r.fulfill({status: 409, json: {error: 'read-only UI fixture'}});
      if (u.pathname === '/api/file') {
        const f = FILES[u.searchParams.get('path')];
        return f ? r.fulfill({json: f}) : r.fulfill({status: 404, json: {error: 'not found'}});
      }
      if (u.pathname === '/api/file/raw' && u.searchParams.get('path') === '/fixture/deck.pdf') {
        return r.fulfill({contentType: 'application/pdf', body: pdf});
      }
      return r.continue();
    });
    await page.route(u => u.hostname === 'example.com', r => r.fulfill({body: 'link target'}));
    await page.goto(base + '/');
    await page.waitForFunction(() => typeof openFilePreview === 'function' && typeof _filePresent === 'function');
    const presentShown = () => page.evaluate(() => getComputedStyle(document.getElementById('file-present-btn')).display !== 'none');

    // A deck with no PDF beside it says how to make it presentable.
    await page.evaluate(() => openFilePreview('/fixture/lonely.pptx'));
    await page.waitForFunction(() => /export it to PDF/.test(document.getElementById('file-body').innerText));
    assert.equal(await presentShown(), false, 'a deck with no PDF export must not offer Present');

    // A deck with its export beside it offers Present on the card and header.
    await page.evaluate(() => openFilePreview('/fixture/deck.pptx'));
    await page.waitForFunction(() => getComputedStyle(document.getElementById('file-present-btn')).display !== 'none');
    assert.match(await page.evaluate(() => document.getElementById('file-body').innerText), /Presents deck\.pdf/);

    await page.click('#file-present-btn');
    await page.waitForFunction(() => window._presentState && window._presentState.total === 3 && window._presentState.rendered > 0);
    const state = await page.evaluate(() => ({...window._presentState, fs: document.fullscreenElement && document.fullscreenElement.id}));
    assert.equal(state.path, '/fixture/deck.pdf');
    assert.equal(state.fs, 'present-overlay', 'Present must enter browser full screen');

    // Every page: the slide fills the screen edge to edge in its own colour,
    // at device pixels, with nothing of the dashboard in the frame.
    for (let n = 1; n <= COLORS.length; n++) {
      await page.waitForFunction(n => document.querySelector('#present-overlay canvas')?.dataset.page === String(n), n);
      const m = await page.evaluate(() => {
        const c = document.querySelector('#present-overlay canvas');
        const r = c.getBoundingClientRect();
        const px = c.getContext('2d').getImageData(Math.floor(c.width / 2), Math.floor(c.height / 2), 1, 1).data;
        return {w: r.width, h: r.height, cw: c.width, ch: c.height, px: [...px].slice(0, 3), dpr: devicePixelRatio};
      });
      assert.ok(Math.abs(m.w - width) <= 1 || Math.abs(m.h - height) <= 1, `slide ${n} must fill the screen on one axis at ${width}x${height}: ${m.w}x${m.h}`);
      assert.ok(Math.abs(m.cw - Math.round(m.w * m.dpr)) <= 1, `slide ${n} must be drawn at device pixels (${m.cw} for ${m.w} css at ${m.dpr}x)`);
      COLORS[n - 1].forEach((v, i) => assert.ok(Math.abs(m.px[i] - v) <= 2, `slide ${n} shows ${m.px}, expected ${COLORS[n - 1]}`));
      const shot = await page.screenshot({clip: {x: width / 2, y: height / 2, width: 1, height: 1}});
      assert.ok(shot.length > 0);
      if (n < COLORS.length) await page.keyboard.press('ArrowRight');
    }

    // Navigation: past the end stays put; Home, Space, Left and both click
    // halves move; the PDF's own link opens a tab without changing the slide.
    await page.keyboard.press('ArrowRight');
    assert.equal(await page.evaluate(() => window._presentState.page), 3);
    const [tab] = await Promise.all([context.waitForEvent('page'), page.click('#present-overlay .present-links a')]);
    assert.equal(tab.url(), 'https://example.com/deck-link');
    await tab.close();
    assert.equal(await page.evaluate(() => window._presentState.page), 3, 'a link click must not advance');
    await page.keyboard.press('Home');
    await page.waitForFunction(() => window._presentState.page === 1);
    await page.keyboard.press('Space');
    await page.waitForFunction(() => window._presentState.page === 2);
    await page.keyboard.press('ArrowLeft');
    await page.waitForFunction(() => window._presentState.page === 1);
    await page.mouse.click(width * 0.5, height * 0.9);
    await page.waitForFunction(() => window._presentState.page === 2);
    await page.mouse.click(width * 0.1, height * 0.5);
    await page.waitForFunction(() => window._presentState.page === 1);

    await page.keyboard.press('Escape');
    await page.waitForFunction(() => !document.getElementById('present-overlay') && !document.fullscreenElement);
    assert.ok(await page.evaluate(() => document.getElementById('file-overlay').classList.contains('active')), 'Esc leaves the file viewer open');

    // Esc pops the history entry Present pushed, so Back is not left pointing at a closed show.
    assert.equal(await page.evaluate(() => !!(history.state && history.state.amuxPresent)), false, 'closing must pop the Back entry Present pushed');

    // Leaving browser full screen any other way (Android Back, the OS gesture)
    // ends the show too.
    await page.click('#file-present-btn');
    await page.waitForFunction(() => window._presentState && window._presentState.rendered > 0 && document.fullscreenElement);
    await page.evaluate(() => document.exitFullscreen());
    await page.waitForFunction(() => !document.getElementById('present-overlay'));

    // A PDF opened directly offers Present too.
    await page.evaluate(() => openFilePreview('/fixture/deck.pdf'));
    await page.waitForFunction(() => getComputedStyle(document.getElementById('file-present-btn')).display !== 'none');
    assert.deepEqual(errors, []);
    console.log(`PASS ${width}x${height}@${dpr}x: 3 slides full screen at device pixels, navigation, link and Esc`);
    await context.close();
  }

  // PHONES (Ethan, 2026-10-25: "make sure that I can exit on mobile"). A phone
  // has no Esc key, and iPhone Safari has no element full screen, so this run
  // removes requestFullscreen the way the iPhone does. The exit must still be
  // on screen while idle, and Back must close the show without leaving the file.
  for (const [width, height] of [[390, 844], [844, 390]]) {
    const context = await browser.newContext({ignoreHTTPSErrors: true, serviceWorkers: 'block',
      viewport: {width, height}, deviceScaleFactor: 3, isMobile: true, hasTouch: true});
    await context.addInitScript(() => {
      localStorage.setItem('amux_walkthrough_done', '1');
      Element.prototype.requestFullscreen = undefined;
      Element.prototype.webkitRequestFullscreen = undefined;
      window.EventSource = class {
        static OPEN = 1; static CLOSED = 2; readyState = 1;
        close() {this.readyState = 2;} addEventListener() {} removeEventListener() {}
      };
    });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', e => errors.push(String(e)));
    await page.route('**/app.js*', r => r.fulfill({contentType: 'text/javascript', body: source}));
    await page.route('**/app.css*', r => r.fulfill({contentType: 'text/css', body: css}));
    await page.route(u => u.pathname === '/' || u.pathname === '/index.html', async r => {
      const resp = await r.fetch();
      let body = await resp.text();
      const [a, b] = [html.indexOf('<div id="file-overlay"'), html.indexOf('<div id="file-body"')];
      const [a2, b2] = [body.indexOf('<div id="file-overlay"'), body.indexOf('<div id="file-body"')];
      if (a > 0 && a2 > 0) body = body.slice(0, a2) + html.slice(a, b) + body.slice(b2);
      return r.fulfill({response: resp, body});
    });
    await page.route('**/api/**', r => {
      const u = new URL(r.request().url());
      if (r.request().method() !== 'GET') return r.fulfill({status: 409, json: {error: 'read-only UI fixture'}});
      if (u.pathname === '/api/file') {
        const f = FILES[u.searchParams.get('path')];
        return f ? r.fulfill({json: f}) : r.fulfill({status: 404, json: {error: 'not found'}});
      }
      if (u.pathname === '/api/file/raw' && u.searchParams.get('path') === '/fixture/deck.pdf') {
        return r.fulfill({contentType: 'application/pdf', body: pdf});
      }
      return r.continue();
    });
    await page.goto(base + '/');
    await page.waitForFunction(() => typeof openFilePreview === 'function' && typeof _filePresent === 'function');
    const urlBefore = page.url();
    const open = async () => {
      await page.evaluate(() => openFilePreview('/fixture/deck.pdf'));
      await page.waitForFunction(() => getComputedStyle(document.getElementById('file-present-btn')).display !== 'none');
      const box = await page.locator('#file-present-btn').boundingBox();
      assert.ok(box && box.x >= 0 && box.x + box.width <= width && box.y >= 0, `Present must be on screen at ${width}x${height}: ${JSON.stringify(box)}`);
      await page.tap('#file-present-btn');
      await page.waitForFunction(() => window._presentState && window._presentState.rendered > 0);
    };
    const swipe = dx => page.evaluate(dx => {
      const ov = document.getElementById('present-overlay');
      const t = x => new Touch({identifier: 1, target: ov, clientX: x, clientY: 200});
      const x0 = innerWidth / 2;
      ov.dispatchEvent(new TouchEvent('touchstart', {touches: [t(x0)], changedTouches: [t(x0)], bubbles: true}));
      ov.dispatchEvent(new TouchEvent('touchend', {touches: [], changedTouches: [t(x0 + dx)], bubbles: true, cancelable: true}));
    }, dx);

    await open();
    const cover = await page.evaluate(() => { const r = document.getElementById('present-overlay').getBoundingClientRect(); return [r.width, r.height, !!document.fullscreenElement]; });
    assert.deepEqual(cover, [width, height, false], `without element full screen the show must still cover the screen at ${width}x${height}`);
    await swipe(-120);
    await page.waitForFunction(() => window._presentState.page === 2);
    await swipe(120);
    await page.waitForFunction(() => window._presentState.page === 1);
    await page.waitForTimeout(2600);   // idle: counter and cursor fade
    const close = await page.evaluate(() => {
      const b = document.querySelector('#present-overlay .present-close');
      const r = b.getBoundingClientRect();
      return {op: +getComputedStyle(b).opacity, x: r.x, y: r.y, w: r.width, h: r.height,
        top: document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2) === b};
    });
    assert.ok(close.op >= 0.4, `the exit button must stay visible while idle on a phone (opacity ${close.op})`);
    assert.ok(close.w >= 44 && close.h >= 44 && close.x + close.w <= width && close.y >= 0 && close.top, `the exit button must be a reachable 44px target: ${JSON.stringify(close)}`);
    await page.tap('#present-overlay .present-close');
    await page.waitForFunction(() => !document.getElementById('present-overlay'));
    assert.equal(await page.evaluate(() => window._presentState), null);
    assert.ok(await page.evaluate(() => document.getElementById('file-overlay').classList.contains('active')), 'the exit button leaves the file viewer open');
    assert.equal(page.url(), urlBefore);

    // Back (the phone's back gesture) closes the show, not the page.
    await open();
    await page.goBack();
    await page.waitForFunction(() => !document.getElementById('present-overlay'));
    assert.ok(await page.evaluate(() => document.getElementById('file-overlay').classList.contains('active')), 'Back leaves the file viewer open');
    assert.equal(page.url(), urlBefore, 'Back must not navigate away');
    assert.deepEqual(errors, []);
    console.log(`PASS phone ${width}x${height}: no element full screen, swipe, exit button visible while idle, tap to exit, Back to exit`);
    await context.close();
  }
} finally {await browser.close();}
