import {test,expect,Page} from './fixtures';
const now=Math.floor(Date.now()/1000);
function report(stale=false) {return {cache_age_s:stale?180:0,cache_ttl_s:60,providers:[
 {id:'claude',label:'Claude',available:true,metered:true,observed_at:now-(stale?180:0),retry_at:now+120,stale,
 windows:[{label:'5-hour session',remaining_percent:63,used_percent:37,resets_at:now+3600},{label:'Weekly · all models',remaining_percent:38,used_percent:62,resets_at:now+86400}]},
 {id:'codex',label:'Codex',available:true,windows:[{label:'7-day',remaining_percent:93,used_percent:7}]},
 {id:'gemini',label:'Gemini',available:false,cause:'account_quota_not_reported',reason:'This authentication mode does not report an account-wide quota.',windows:[]},
 {id:'ollama',label:'Ollama',available:true,metered:false,windows:[]}
]};}
async function open(page:Page) {
 await page.addInitScript(()=>{localStorage.setItem('amux_walkthrough_done','1');localStorage.setItem('amux_settings_tab','account');});
 await page.clock.install();
 await page.goto('/'); await page.locator('#settings-btn').click();
 await expect(page.locator('#settings-usage-body .usage-provider')).toHaveCount(4);
}
const claude=(page:Page)=>page.locator('.usage-provider[data-provider="claude"]');
test('last-known usage stays readable and automatically becomes fresh',async({page},info)=>{
 let calls=0;
 await page.route(/\/api\/usage$/,r=>{calls++;return r.fulfill({json:report(calls===1)});});
 await open(page);
 await expect(claude(page).locator('summary')).toContainText('Last known · 38% left');
 await expect(claude(page)).toContainText('Last checked 3 min ago');
 await expect(claude(page)).toContainText('Next check in 2 min');
 await expect(claude(page).locator('[data-usage-window]')).toHaveCount(2);
 await expect(page.locator('.usage-provider[data-provider="gemini"] summary')).toContainText('Not reported');
 await page.screenshot({path:info.outputPath('usage-last-known.png')});
 await page.clock.fastForward(30001);
 await expect(claude(page).locator('summary')).toHaveText(/Claude38% left/);
 await expect(claude(page)).not.toContainText('Last known'); expect(calls).toBe(2);
});
test('cold throttling schedules recovery without inventing quota',async({page})=>{
 let calls=0;
 await page.route(/\/api\/usage$/,r=>{calls++;const d=report();if(calls===1)(d.providers as any)[0]={id:'claude',label:'Claude',available:false,cause:'rate_limited',retry_at:now+120,windows:[]};return r.fulfill({json:d});});
 await open(page);
 await claude(page).locator('summary').click();
 await expect(claude(page)).toContainText('Checking…');
 await expect(claude(page)).toContainText('Next check');
 await expect(claude(page).locator('[data-usage-window]')).toHaveCount(0);
 await page.clock.fastForward(30001);
 await expect(claude(page)).toContainText('38% left');
});
test('network interruption keeps the prior reading and closing the panel stops polling',async({page})=>{
 let calls=0;
 await page.route(/\/api\/usage$/,r=>{calls++;return calls===1?r.fulfill({json:report()}):r.abort('failed');});
 await open(page); await page.clock.fastForward(30001);
 await expect(claude(page)).toContainText('38% left');
 await expect(page.locator('.usage-connection-note')).toContainText('reconnecting automatically');
 await page.locator('#settings-btn').click(); const before=calls;
 await page.clock.fastForward(60001);expect(calls).toBe(before);
});
test('a stale reading from an ended window is labelled historical',async({page})=>{
 await page.route(/\/api\/usage$/,r=>{const d=report(true);(d.providers as any)[0].windows=[{label:'5-hour session',remaining_percent:10,used_percent:90,resets_at:now-60}];return r.fulfill({json:d});});
 await open(page);await expect(claude(page)).toContainText('Previous window');
 await expect(claude(page)).toContainText('Last reported: 10% left');
});
