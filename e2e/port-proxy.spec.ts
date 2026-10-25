import { test, expect } from './fixtures';
import * as http from 'http';
import type { AddressInfo } from 'net';

/**
 * Owner, 2026-10-17, from a remote Tailscale client: "these proxies that are
 * presented dont work". Workers and the dashboard hand out
 * <origin>/proxy/<port>/... links for dev servers on this machine.
 *
 * Two real local apps are reached the way a remote device does:
 * - one the browser can reach on its own port is REDIRECTED there;
 * - one bound to loopback only is RELAYED through amux: the page renders, its
 *   absolute asset paths load, a video answers a Range request with 206, a
 *   redirect stays inside the proxy, and amux's owner cookie never reaches it.
 * With no amux sign-in neither is served. The peek turns a worker's
 * http://localhost link into a proxy link that opens the app in a new tab.
 */
const PIXEL = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==', 'base64');
const CLIP = Buffer.alloc(4096, 7);

function app(seenCookies: string[]) {
  return http.createServer((req, res) => {
    seenCookies.push(String(req.headers.cookie || ''));
    const url = req.url || '/';
    if (url === '/') {
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Set-Cookie': 'appsid=1; Path=/' });
      res.end('<html><body><h1 id="t">proxied app ok</h1><img id="i" src="/pixel.png"><a id="l" href="/redir">go</a></body></html>');
    } else if (url === '/pixel.png') {
      res.writeHead(200, { 'Content-Type': 'image/png' }); res.end(PIXEL);
    } else if (url === '/clip.webm') {
      const m = /bytes=(\d+)-(\d+)/.exec(String(req.headers.range || ''));
      if (m) {
        const [a, b] = [Number(m[1]), Number(m[2])];
        res.writeHead(206, { 'Content-Type': 'video/webm', 'Content-Range': `bytes ${a}-${b}/${CLIP.length}`, 'Accept-Ranges': 'bytes' });
        res.end(CLIP.subarray(a, b + 1));
      } else { res.writeHead(200, { 'Content-Type': 'video/webm', 'Accept-Ranges': 'bytes' }); res.end(CLIP); }
    } else if (url === '/redir') {
      res.writeHead(302, { Location: '/landed' }); res.end();
    } else if (url === '/landed') {
      res.writeHead(200, { 'Content-Type': 'text/html' }); res.end('<h1 id="t">landed inside the proxy</h1>');
    } else { res.writeHead(404); res.end('nope'); }
  });
}

async function listen(s: http.Server, host: string): Promise<number> {
  await new Promise<void>((r, j) => { s.once('error', j); s.listen(0, host, () => r()); });
  return (s.address() as AddressInfo).port;
}

test('proxy links open a local app from a signed-in remote browser, and not without sign-in', async ({ page, browser, baseURL }) => {
  const reachableSeen: string[] = [];
  const loopbackSeen: string[] = [];
  const reachable = app(reachableSeen);
  const loopbackOnly = app(loopbackSeen);
  const rPort = await listen(reachable, '127.0.0.1');
  // IPv6 loopback only: a browser that opened amux at 127.0.0.1 cannot reach
  // it directly, which is the loopback-only dev server case.
  const lPort = await listen(loopbackOnly, '::1');
  const amuxPort = new URL(baseURL!).port;
  const relayOrigin = `https://127.0.0.1:${amuxPort}`;
  try {
    await page.goto('/');
    await page.waitForFunction(() => !!(window as any)._AMUX_AUTH_TOKEN, { timeout: 20000 });
    const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN as string);
    const rewritten = await page.evaluate((p) => (window as any).rewriteLocalhostUrls(`see http://localhost:${p}/ now`), rPort);
    expect(rewritten).toContain(`${new URL(baseURL!).origin}/proxy/${rPort}/`);

    // A remote browser that never signed in to amux is sent to sign in, and
    // neither app is reached.
    const remote = await browser.newContext({ ignoreHTTPSErrors: true, baseURL });
    for (const [origin, p] of [[baseURL!, rPort], [relayOrigin, lPort]] as const) {
      const anon = await remote.request.get(`${origin}/proxy/${p}/`, { maxRedirects: 0 });
      expect([302, 303, 307], `${origin} ${await anon.text()}`).toContain(anon.status());
      expect(anon.headers()['location']).toBe('/');
    }
    expect(reachableSeen.length + loopbackSeen.length, 'a refused request reached an app').toBe(0);

    // The owner signs in on that device, on both origins (cookies are per host).
    const rp = await remote.newPage();
    for (const origin of [baseURL!, relayOrigin]) {
      await rp.goto(`${origin}/?_token=` + encodeURIComponent(token));
      await rp.waitForURL(/_fresh=1/, { timeout: 20000 });
    }

    // Reachable on its own port: sent there, so scripts and HMR work as locally.
    await rp.goto(`/proxy/${rPort}/`);
    await expect(rp.locator('#t')).toHaveText('proxied app ok');
    expect(new URL(rp.url()).port).toBe(String(rPort));

    // Loopback only: relayed through amux.
    await rp.goto(`${relayOrigin}/proxy/${lPort}/`);
    await expect(rp.locator('#t')).toHaveText('proxied app ok');
    expect(new URL(rp.url()).origin).toBe(relayOrigin);
    await expect(rp.locator('#i')).toHaveAttribute('src', `/proxy/${lPort}/pixel.png`);
    await expect.poll(() => rp.locator('#i').evaluate((i: HTMLImageElement) => i.naturalWidth)).toBe(1);
    const range = await rp.evaluate(async (p) => {
      const r = await fetch(`/proxy/${p}/clip.webm`, { headers: { Range: 'bytes=0-9' } });
      return { status: r.status, len: (await r.arrayBuffer()).byteLength, cr: r.headers.get('content-range') };
    }, lPort);
    expect(range).toEqual({ status: 206, len: 10, cr: `bytes 0-9/${CLIP.length}` });
    // The link is rewritten into the proxy, and the app's redirect stays there.
    // Followed by navigation: a WebKit tap on it raced the assertion on main
    // (ios-safari, 92b1c9660 and 294c658ff), and the tap is not what is tested.
    await expect(rp.locator('#l')).toHaveAttribute('href', `/proxy/${lPort}/redir`);
    await rp.goto(`${relayOrigin}/proxy/${lPort}/redir`);
    await expect(rp.locator('#t')).toHaveText('landed inside the proxy');
    expect(new URL(rp.url()).pathname).toBe(`/proxy/${lPort}/landed`);
    // The app's own cookie round-trips; amux's owner cookie never reaches it.
    await rp.goto(`${relayOrigin}/proxy/${lPort}/`);
    await expect(rp.locator('#t')).toHaveText('proxied app ok');
    expect(loopbackSeen.some(c => c.includes('appsid=1')), loopbackSeen.join(' | ')).toBe(true);
    expect(loopbackSeen.some(c => /amux/i.test(c)), loopbackSeen.join(' | ')).toBe(false);

    // The way the owner meets it: a worker's output in the peek, on the remote
    // device, rendered by the shipped pipeline, link clicked into a new tab.
    await rp.goto('/');
    await rp.waitForFunction(() => typeof (window as any).openPeek === 'function', { timeout: 20000 });
    await rp.evaluate(() => (window as any).openPeek('e2e-probe'));
    await rp.waitForSelector('#peek-overlay', { state: 'visible', timeout: 15000 });
    const href = await rp.evaluate((p) => {
      const body = document.getElementById('peek-body')!;
      body.innerHTML = (window as any)._peekHtml(`Dev server ready: http://localhost:${p}/ (open it)`);
      const a = body.querySelector(`a[href*="/proxy/${p}/"]`) as HTMLAnchorElement | null;
      return a ? a.href : 'no link in: ' + body.innerHTML;
    }, rPort);
    expect(href).toBe(`${new URL(baseURL!).origin}/proxy/${rPort}/`);
    // It opens in a new tab. Opened by navigation: a WebKit tap on a
    // target=_blank link hung the click itself on ios-safari.
    await expect(rp.locator(`#peek-body a[href*="/proxy/${rPort}/"]`).first()).toHaveAttribute('target', '_blank');
    const tab = await remote.newPage();
    await tab.goto(href);
    await expect(tab.locator('#t')).toHaveText('proxied app ok', { timeout: 15000 });

    // A port nothing listens on says so instead of serving the dashboard.
    const dead = await rp.evaluate(async () => { const r = await fetch('/proxy/1/'); return { s: r.status, b: await r.text() }; });
    expect(dead.s).toBe(502);
    expect(dead.b).toContain('Nothing is running on that port');
    await remote.close();
  } finally {
    reachable.close();
    loopbackOnly.close();
  }
});
