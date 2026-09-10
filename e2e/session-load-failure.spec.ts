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
    const notice = page.locator('#session-read-notice');
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
    await expect(page.locator('#session-read-notice')).toContainText('Worker updates are unavailable');
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
  const notice = page.locator('#session-read-notice');
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
  await expect(page.locator('#session-read-notice')).toContainText('Access to this workspace needs to be renewed');
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
    await expect(page).toHaveURL(/\?_fresh=auth#view=workers/);
    await expect.poll(() => statuses.some(s => s === 401)).toBe(true);
    await expect.poll(() => statuses.some(s => s === 200)).toBe(true);
    await expect(page.locator('#session-read-notice')).toBeEmpty();
    // Recovery must display the server's real roster. This project may already
    // contain workers created by earlier scenarios; bootstrap is not a reset.
    const names = await page.evaluate(async () => {
      const response = await fetch('/api/sessions');
      if (!response.ok) throw new Error('recovered roster HTTP ' + response.status);
      return (await response.json()).map((row: any) => row.name);
    });
    if (!names.length) await expect(page.locator('#cards')).toContainText('No workers yet');
    else for (const name of names) await expect(page.locator('#cards')).toContainText(name);
    await expect(page.locator('#conn-status').first()).toHaveText(/Live|Polling/);
  });
}
