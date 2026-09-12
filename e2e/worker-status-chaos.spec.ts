import {test, expect} from './fixtures';

for (const [provider, model] of [['claude','claude-sonnet-5'],['codex','gpt-5.6-luna'],['gemini','gemini-2.5-flash-lite']]) {
  test(`${provider} status labels and recovery remain distinct at low effort`, async ({page},info) => {
    const worker = {name:'status-chaos',provider,model,running:true,status:'idle',dir:'/tmp',effort:'low'};
    await page.addInitScript(() => localStorage.setItem('amux_walkthrough_done','1'));
    await page.route(/\/api\/sessions(?:\?.*)?$/, r => r.fulfill({json:[worker]}));
    await page.route('**/api/sessions/status-chaos/peek?*', r => r.fulfill({json:{name:worker.name,live:'Ready',pane_cols:80}}));
    await page.goto('/');
    await page.waitForFunction(() => typeof (window as any).openPeek === 'function');
    await page.evaluate(worker => {
      eval('sessions=['+JSON.stringify(worker)+'];');
      (window as any).openPeek(worker.name); (window as any)._stopPeekPoll();
    },worker);
    const cases = [
      {status:'starting',running:false,label:'starting',key:'starting'},
      {status:'active',label:'working',key:'working'},
      {status:'waiting',waiting_reason:'',label:'waiting',key:'waiting'},
      {status:'waiting',waiting_reason:'user_input',label:'needs input',key:'waiting'},
      {status:'blocked',waiting_reason:'permission_prompt',label:'blocked',key:'blocked'},
      {status:'rate_limited',waiting_reason:'rate_limit',label:'rate limited',key:'rate_limited'},
      {status:'api_error',api_error:true,api_error_code:529,label:'API 529',key:'api_error'},
      {status:'error',label:'error',key:'error'},
      {status:'idle',label:'idle',key:'idle'},
      {status:'active',running:false,label:'stopped',key:'stopped'},
      {status:'idle',label:'idle',key:'idle'},
    ];
    for (const state of cases) {
      const current = {...worker,...state};
      const key = await page.evaluate(current => {
        eval('sessions=['+JSON.stringify(current)+']; render(); updatePeekStatus();');
        return (window as any)._sessStatusKey(current);
      },current);
      expect(key).toBe(state.key);
      await expect(page.locator('#peek-session-status')).toContainText(state.label);
      if (state.key !== 'waiting') await expect(page.locator('#peek-session-status')).not.toContainText('needs input');
      await expect(page.locator('#cards .card[data-session="status-chaos"] .status-badge').first()).toContainText(state.label);
    }
    // Older servers may still return waiting/rate_limit: never send a false
    // human-input notification while the user is automatically waiting.
    const notifications = await page.evaluate(worker => {
      const calls: string[] = [];
      (window as any)._fireSessionNotif = (...args:string[]) => calls.push(args.join(' '));
      eval('Object.keys(_prevSessionState).forEach(k => delete _prevSessionState[k]); _initialLoad = false;');
      eval('sessions=['+JSON.stringify({...worker,status:'active'})+']; _checkSessionTransitions(sessions);');
      eval('sessions=['+JSON.stringify({...worker,status:'waiting',waiting_reason:'rate_limit'})+']; _checkSessionTransitions(sessions);');
      return calls;
    },worker);
    expect(notifications).toEqual([]);
    await page.screenshot({path:info.outputPath('worker-status-recovered.png')});
  });
}
