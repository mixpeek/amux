import { test, expect, Page } from './fixtures';

async function anonymousShell(page: Page, cached = false) {
  await page.addInitScript(({ cached }) => {
    localStorage.setItem('amux_walkthrough_done', '1');
    // A remote, unauthenticated shell intentionally contains no owner token.
    Object.defineProperty(window, '_AMUX_AUTH_TOKEN', { get: () => '', set: () => {}, configurable: true });
    if (cached) localStorage.setItem('amux_sessions_cache', JSON.stringify([
      {name:'saved-worker',dir:'/tmp/example',status:'idle',running:true}
    ]));
  }, { cached });
  await page.route('**/?_fresh=auth', route => route.fulfill({contentType:'text/html',
    body:'<!-- AMUX-BOOTSTRAP-BEGIN --><script>window._AMUX_AUTH_TOKEN="";</script><!-- AMUX-BOOTSTRAP-END -->'}));
}

for (const cached of [false, true]) {
  test(`401 resolves loading and preserves the access error${cached ? ' over cached workers' : ''}`, async ({page}, info) => {
    await anonymousShell(page, cached);
    let allowed = false;
    const recovered: any[] = [];
    let freshRequests = 0;
    page.on('request', r => { if (new URL(r.url()).searchParams.has('_fresh')) freshRequests++; });
    await page.route('**/api/**', route => {
      const path = new URL(route.request().url()).pathname;
      if (allowed && path === '/api/sessions') return route.fulfill({json:[]});
      if (allowed && path === '/api/client-debug') {
        const d = route.request().postDataJSON();
        if (d?.kind === 'session-load-failure') recovered.push(d);
        return route.fulfill({json:{ok:true}});
      }
      return route.fulfill({status:401,json:{error:'unauthorized',reason:'missing_credential'}});
    });
    await page.goto('/');
    await expect(page.locator('#session-read-notice')).toBeEmpty();
    await page.locator('#conn-status').first().click();
    const notice = page.locator('#conn-modal-read-notice');
    await expect(notice).toContainText('Access to this workspace needs to be renewed');
    await expect(page.locator('#conn-status').first()).toHaveText('Access required');
    await expect(page.locator('#cards')).not.toContainText('Connecting to server');
    await expect(page.locator('#cards')).not.toContainText('No workers yet');
    if (cached) {
      await expect(page.locator('#cards')).toContainText('saved-worker');
      await expect(notice).toContainText('last saved copy');
    }
    await notice.getByText('Connection details').click();
    await expect(notice).toContainText('HTTP 401 · missing_credential');
    await page.evaluate(() => (window as any).fetchSessions());
    await expect(notice.locator('details')).toHaveAttribute('open', '');
    await expect.poll(() => freshRequests).toBe(1); // no reload or recovery storm
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
    await page.screenshot({path:info.outputPath(`access-required-${cached ? 'cached' : 'empty'}.png`)});
    allowed = true;
    await notice.getByRole('button', {name:'Retry connection'}).click();
    await expect(notice).toBeEmpty();
    await expect(page.locator('#cards')).toContainText('No workers yet');
    await expect.poll(() => recovered.length).toBe(1);
    expect(recovered[0]).toMatchObject({status:401,reason:'missing_credential',measured:true,n_considered:1,bearer_present:false});
    expect(recovered[0].recovered_at).toBeGreaterThanOrEqual(recovered[0].ts);
    expect(JSON.stringify(recovered[0])).not.toMatch(/Authorization|_token|cookie|query/);
  });
}

for (const failure of [
  {status:503,body:'{"error":"unavailable"}',reason:'http_error'},
  {status:200,body:'<html>proxy error</html>',reason:'invalid_json'},
  {status:200,body:'{"unexpected":"object"}',reason:'invalid_payload'},
]) {
  test(`${failure.status} ${failure.reason} is a failed read, not a perpetual spinner`, async ({page}) => {
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done', '1'));
    await page.route(/\/api\/sessions(?:\?.*)?$/, route => route.fulfill({status:failure.status,
      contentType:'application/json',body:failure.body}));
    await page.goto('/');
    await expect(page.locator('#session-read-notice')).toBeEmpty();
    await page.locator('#conn-status').first().click();
    await expect(page.locator('#conn-modal-read-notice')).toContainText('Worker updates are unavailable');
    await expect(page.locator('#conn-status').first()).toHaveText('Sync error');
    await expect(page.locator('#cards')).not.toContainText('Connecting to server');
    const evidence = await page.evaluate(() => JSON.parse(sessionStorage.getItem('amux_session_load_failure') || '{}'));
    expect(evidence).toMatchObject({status:failure.status,reason:failure.reason});
  });
}

test('a transient worker-list overload heals without making the user click Retry', async ({page}) => {
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    // Keep realtime healthy-but-quiet so no SSE reconnect/fallback/focus path
    // can supply the request this test attributes to the backoff timer.
    Object.defineProperty(window, 'EventSource', {configurable:true, value:class {
      onmessage = null; onerror = null;
      constructor(_url: string) {}
      close() {}
    }});
  });
  let attempts = 0;
  await page.route(/\/api\/sessions(?:\?.*)?$/, route => {
    attempts++;
    if (attempts === 1) {
      return route.fulfill({status:503, contentType:'application/json',
        body:'{"error":"temporarily unavailable"}'});
    }
    return route.fulfill({status:200, contentType:'application/json', body:'[]'});
  });
  await page.goto('/#view=workers');
  await expect(page.locator('#session-read-notice')).toBeEmpty();
  await page.locator('#conn-status').first().click();
  const notice = page.locator('#conn-modal-read-notice');
  await expect(notice).toContainText('Worker updates are unavailable');
  await expect.poll(() => attempts).toBe(2);
  await expect(notice).toBeEmpty();
  await expect(page.locator('#conn-status').first()).toHaveText(/Live|Polling/);
});

test('an authorization failure does not start the transient-error retry loop', async ({page}) => {
  await page.addInitScript(() => {
    localStorage.setItem('amux_walkthrough_done', '1');
    Object.defineProperty(window, 'EventSource', {configurable:true, value:class {
      onmessage = null; onerror = null;
      constructor(_url: string) {}
      close() {}
    }});
  });
  let attempts = 0;
  await page.route(/\?_fresh=auth(?:#.*)?$/, route => route.abort());
  await page.route(/\/api\/sessions(?:\?.*)?$/, route => {
    attempts++;
    return route.fulfill({status:401, contentType:'application/json',
      body:'{"error":"unauthorized","reason":"invalid_bearer"}'});
  });
  await page.goto('/#view=workers');
  await expect(page.locator('#session-read-notice')).toBeEmpty();
  await page.locator('#conn-status').first().click();
  await expect(page.locator('#conn-modal-read-notice')).toContainText('Access to this workspace needs to be renewed');
  await page.waitForTimeout(1500);
  expect(attempts).toBe(1);
});

for (const initial of ['', 'obsolete-test-credential']) {
  test(`fresh authorized bootstrap repairs ${initial ? 'stale' : 'missing cached'} bearer and preserves location`, async ({page}) => {
    await page.addInitScript(({ initial }) => {
      localStorage.setItem('amux_walkthrough_done', '1');
      if (!new URL(location.href).searchParams.has('_fresh')) {
        Object.defineProperty(window, '_AMUX_AUTH_TOKEN', {get:()=>initial,set:()=>{},configurable:true});
      }
    }, { initial });
    // Use the real isolated server's auth and bootstrap. It disables loopback
    // API admission; a cached bad/empty bearer MUST fail before recovery.
    const statuses: number[] = [];
    page.on('response', r => { if (new URL(r.url()).pathname === '/api/sessions') statuses.push(r.status()); });
    await page.goto('/#view=workers');
    // URL equality can become true before the replacement document loads.
    // Wait for that document before evaluating its roster (AMUX-4416).
    await page.waitForURL(/\?_fresh=auth#view=workers/, {waitUntil:'load'});
    await expect.poll(() => statuses.some(s => s === 401)).toBe(true);
    // Bootstrap recovery itself has a 12s deadline; a 5s assertion truncated it.
    await expect.poll(() => statuses.some(s => s === 200), {timeout:15000}).toBe(true);
    await expect(page.locator('#session-read-notice')).toBeEmpty();
    // Recovery must display the server's real roster. This project may already
    // contain workers created by earlier scenarios; bootstrap is not a reset.
    // The app's own fetch already reached 200 above; this is a second, bare
    // probe that can momentarily race the freshly applied credential on webkit
    // (ios-safari flake that reddened main on an innocent merge, AMUX-6142).
    // Poll it within the same bootstrap deadline rather than failing on a
    // sub-second race; a genuinely unrecovered roster still times out and fails.
    // Each probe has its own 3s deadline: on webkit a probe whose request was
    // cancelled (status -1 in the trace) never settled, and that one pending
    // evaluate spent the whole 15s poll without a retry (run 40514068471).
    let names: string[] = [];
    let lastProbe = 'none';
    await expect.poll(async () => {
      const probe = await page.evaluate(async () => {
        const read = (async () => {
          const response = await fetch('/api/sessions', {cache:'no-store'});
          if (!response.ok) return {outcome:`http ${response.status}`, roster:null};
          return {outcome:'ok', roster:(await response.json()).map((row: any) => row.name)};
        })().catch((error) => ({outcome:`error ${error?.name || error}`, roster:null}));
        const deadline = new Promise<{outcome:string, roster:null}>((resolve) =>
          setTimeout(() => resolve({outcome:'no answer in 3s', roster:null}), 3000));
        return Promise.race([read, deadline]);
      });
      lastProbe = probe.outcome;
      if (probe.roster) names = probe.roster;
      return probe.roster !== null;
    }, {timeout:15000, message:'recovered roster read (credential race)'}).toBe(true)
      .catch((error) => { throw new Error(`${error.message}\nlast probe: ${lastProbe}`); });
    if (!names.length) await expect(page.locator('#cards')).toContainText('No workers yet');
    else for (const name of names) await expect(page.locator('#cards')).toContainText(name);
    await expect(page.locator('#conn-status').first()).toHaveText(/Live|Polling/);
  });
}
