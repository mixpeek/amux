import {test, expect, Page} from './fixtures';
const subs=[{id:'agent-one',conversation:'parent-a',description:'Check the imports',last_active:1},{id:'agent-two',conversation:'parent-a',description:'Review the tests',last_active:2}];
async function setup(page:Page, items=subs, failFirst=false) {
  await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
  await page.route(/\/api\/sessions(?:\?.*)?$/,r=>r.fulfill({json:[{name:'arrows',running:true,status:'idle',dir:'/tmp/arrows'}]}));
  await page.route('**/api/sessions/arrows/peek?*',r=>r.fulfill({json:{name:'arrows',live:'Main worker terminal output',history:''}}));
  await page.route('**/api/sessions/arrows/subagents',r=>{if(failFirst){failFirst=false;return r.fulfill({status:503,json:{error:'Unavailable'}});}return r.fulfill({json:{session:'arrows',subagents:items}});});
  await page.goto('/');
  await page.waitForFunction(()=>typeof (window as any).openPeek==='function');
  await page.evaluate(()=>(window as any).openPeek('arrows'));
  await expect(page.locator('#peek-overlay')).toHaveCSS('opacity','1');
  await expect(page.locator('#peek-body')).toContainText('Main worker terminal output');
}
test('terminal arrows cycle actual child output and preserve the parent draft',async({page},info)=>{
  const writes:string[]=[];
  page.on('request',r=>{if(/\/api\/sessions\/arrows\//.test(r.url())&&r.method()!=='GET')writes.push(r.url());});
  await page.route('**/api/sessions/arrows/subagents?*',r=>{
    const agent=new URL(r.request().url()).searchParams.get('agent');
    return r.fulfill({json:{session:'arrows',agent,conversation:'parent-a',output:'⏺ Output from '+agent}});
  });
  await setup(page);
  await expect(page.locator('#peek-subagents-btn')).toHaveCount(0);
  const nav=page.getByRole('navigation',{name:'Worker and subagent output'});
  await expect(nav).toBeVisible();
  const bounds=await nav.boundingBox(), terminal=await page.locator('.peek-output-wrap').boundingBox();
  expect(bounds!.x+bounds!.width).toBeGreaterThan(terminal!.x+terminal!.width-25);
  expect(bounds!.y).toBeLessThan(terminal!.y+15);
  await page.locator('#peek-cmd-input').fill('Keep my draft');
  await nav.getByRole('button',{name:'Next agent',exact:true}).click();
  await expect(page.locator('#peek-body')).toContainText('Output from agent-one');
  await page.evaluate(()=>(window as any).refreshPeek());
  await expect(page.locator('#peek-body')).toContainText('Output from agent-one');
  await nav.getByRole('button',{name:'Next agent',exact:true}).click();
  await expect(page.locator('#peek-body')).toContainText('Output from agent-two');
  await page.screenshot({path:info.outputPath('terminal-subagent-arrows.png')});
  await nav.getByRole('button',{name:'Next agent',exact:true}).click();
  await expect(page.locator('#peek-body')).toContainText('Main worker terminal output');
  await expect(page.locator('#peek-cmd-input')).toHaveValue('Keep my draft');
  await nav.getByRole('button',{name:'Previous agent',exact:true}).click();
  await expect(page.locator('#peek-body')).toContainText('Output from agent-two');
  expect(writes).toEqual([]);
});
test('workers without subagents hide the two-arrow control',async({page})=>{
  await setup(page,[]);
  await expect(page.locator('#peek-agent-nav')).toBeHidden();
});
test('failed child output is explicit and the arrows still return to the parent',async({page})=>{
  await page.route('**/api/sessions/arrows/subagents?*',r=>r.fulfill({status:503,json:{error:'Unavailable'}}));
  await setup(page);
  await page.getByRole('button',{name:'Next agent',exact:true}).click();
  await expect(page.locator('#peek-body')).toContainText('Could not load subagent output');
  await page.getByRole('button',{name:'Previous agent',exact:true}).click();
  await expect(page.locator('#peek-body')).toContainText('Main worker terminal output');
});
test('a late child response cannot replace the main worker after switching back',async({page})=>{
  let release!:()=>void;
  const held=new Promise<void>(resolve=>release=resolve);
  await page.route('**/api/sessions/arrows/subagents?*',async r=>{await held;await r.fulfill({json:{session:'arrows',agent:'agent-one',conversation:'parent-a',output:'STALE CHILD OUTPUT'}});});
  await setup(page);
  const request=page.waitForRequest(r=>r.url().includes('/subagents?agent='));
  await page.getByRole('button',{name:'Next agent',exact:true}).click();
  await request;
  await page.getByRole('button',{name:'Previous agent',exact:true}).click();
  const response=page.waitForResponse(r=>r.url().includes('/subagents?agent='));
  release(); await response;
  await expect(page.locator('#peek-body')).toContainText('Main worker terminal output');
  await expect(page.locator('#peek-body')).not.toContainText('STALE CHILD OUTPUT');
});

test('failed agent discovery exposes arrow retry',async({page})=>{
  await page.route('**/api/sessions/arrows/subagents?*',r=>r.fulfill({json:{session:'arrows',agent:'agent-one',conversation:'parent-a',output:'Recovered child output'}}));
  await setup(page,subs,true);
  await expect(page.locator('#peek-agent-label')).toHaveText('Agents unavailable');
  await page.getByRole('button',{name:'Next agent',exact:true}).click();
  await expect(page.locator('#peek-body')).toContainText('Recovered child output');
});
