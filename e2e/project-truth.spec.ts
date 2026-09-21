import {test, expect, allowUnusedRoute} from './fixtures';

// Focused truth/state fixtures for the Projects task inspector (AAB-11 review). The real-server
// lifecycle in e2e/project-lifecycle/ui.mjs covers the end-to-end path; these drive exact states.
type Card = {id:string;title:string;phase:string;stage?:string;waiting?:string|null;label?:string;lastFailure?:string|null;worker?:string};
const policy={enabled:true,paused:false,repository:'/tmp/r',coordinator:{provider:'claude',model:'haiku'},executor:{provider:'claude',model:'sonnet'},verify_command:'true',verification_timeout_secs:60,max_executors:1,max_attempts:2};
const card=(c:Card)=>({id:c.id,title:c.title,phase:c.phase,rev:1,next_action:'next',acceptance_criteria:['Output passes'],evidence:c.phase==='verified'?JSON.stringify({report:{head:'a'.repeat(40)},merged:'b'.repeat(40),gate:'true'}):'',
  execution_plan:{waiting_reason:c.waiting??null,waiting_label:c.label??null,execution:{stage:c.stage??'working',attempt:1,generation:2,worker:c.worker??'',report:null,retained_assets:[],last_failure:c.lastFailure??null,input_hash:'h'}}});
const world={cards:[] as Card[],failAll:false,failFirst:0,sessions:[] as unknown[],inventory:'ok' as 'ok'|'fail',expired:[] as string[],requests:0,acceptance:null as any,approval:null as any};
const detail=(name:string)=>({project:{name,revision:1,policy},usage:{verified_outcomes:0,requested_outcomes:0,execution_attempts:0,intake_calls:0,measured:false,tokens:0,intake_calls_measured:0,execution_turns_measured:0,cost_measured:false},commands:[],cards:world.cards.map(card),pause_settled:true,migrations:[],acceptance:world.acceptance||{state:'not_configured',measured:true,n_considered:0}});

test.beforeEach(async ({page})=>{
  Object.assign(world,{cards:[],failAll:false,failFirst:0,sessions:[],inventory:'ok',expired:[],requests:0,acceptance:null,approval:null});
  await page.addInitScript(()=>{
    localStorage.setItem('amux_walkthrough_done','1');localStorage.setItem('amux_project_selected','px');
    (window as any)._PROJECT_REFRESH_MS=100;(window as any)._PROJECT_REFRESH_CAP_MS=300;
  });
  await page.route(/\/api\/projects(\/p[xy])?$/,async r=>{
    world.requests++;
    if(world.failAll || world.failFirst>0){world.failFirst=Math.max(0,world.failFirst-1);return r.abort();}
    const name=new URL(r.request().url()).pathname.split('/')[3];
    return r.fulfill({json:name?detail(name):{projects:[{name:'px'},{name:'py'}]}});
  });
  await page.route('**/api/sessions',r=>r.fulfill({json:world.sessions}));
  await page.route('**/api/board/orchestrations',r=>world.inventory==='fail'?r.fulfill({status:500,json:{error:'down'}}):r.fulfill({json:{measured:true,workers:world.expired.map(name=>({name,lifecycle:'expired'}))}}));
  allowUnusedRoute(page,'**/api/sessions');allowUnusedRoute(page,'**/api/board/orchestrations'); // read only when a worker is inspected
});
const open=async(page:any)=>{await page.goto('/');await page.locator('#tab-projects').click();await page.locator('#project-cards .project-card-select').first().waitFor();};
const pick=async(page:any,id:string)=>{await page.locator('[data-task="'+id+'"] .project-card-select').click();};
const refreshed=async(page:any)=>{const t=await page.evaluate('_projectsToken') as number;await page.waitForFunction('_projectsToken>='+(t+2));};

test('closed tasks are counted separately and never as verified',async ({page})=>{
  world.cards=[{id:'T-1',title:'done',phase:'verified',stage:'verified'},{id:'T-2',title:'dropped',phase:'closed',stage:'closed'},{id:'T-3',title:'live',phase:'working'}];
  await open(page);
  const text=await page.locator('#project-progress').innerText();
  expect(text).toContain('1 of 3 tasks verified');expect(text).toContain('1 closed (not verified)');
});

test('whole-project acceptance is reviewable and human approval is bound to its fingerprint',async ({page})=>{
  await page.route('**/api/projects/*/acceptance/approve',async r=>{world.approval=r.request().postDataJSON();await r.fulfill({json:detail('px')});});
  world.cards=[{id:'T-1',title:'verified output',phase:'verified',stage:'verified'}];
  const retained={source:{path:'demo.webm',sha256:'c'.repeat(64)},path:'/private/artifacts/project-reports/'+ 'c'.repeat(64)+'.webm'};
  world.acceptance={state:'awaiting_human',fingerprint:'f'.repeat(64),main:'a'.repeat(40),review_assets:[{task:'T-1',title:'verified output',worker:'px-a',asset:retained}],criteria:[
    {id:'e2e',requirement:'Happy and unhappy paths pass',verifier:{type:'command',id:'e2e-suite'},result:{state:'passed',output:'14 scenarios passed',evidence:[]}},
    {id:'owner',requirement:'Owner can inspect the demo',verifier:{type:'human',id:'owner-review'},result:{state:'pending_human',evidence:[]}},
  ]};
  await open(page);
  const acceptance=page.locator('#project-acceptance');
  await expect(acceptance).toContainText('awaiting_human');
  await expect(page.locator('#project-state')).toContainText('executors retained without running');
  await expect(acceptance).toContainText('Produced artifacts to review');
  await expect(acceptance.getByRole('button',{name:'T-1 · demo.webm'})).toBeVisible();
  await expect(acceptance).toContainText('14 scenarios passed');
  await acceptance.getByRole('button',{name:'Approve'}).click();
  await expect.poll(()=>world.approval).toEqual({criterion:'owner',fingerprint:'f'.repeat(64),decision:'approve',note:''});
});

test('a failure is current only while the task is waiting; older failures are labelled historical',async ({page})=>{
  world.cards=[{id:'T-1',title:'repair',phase:'waiting',stage:'waiting',waiting:'Verification failed\nline two',label:'Verification failed',lastFailure:'gen2 Version locator failure'}];
  await open(page);await pick(page,'T-1');
  const box=page.locator('#project-inspector');
  await expect(box.locator('.project-failure')).toContainText('Current issue');
  world.cards=[{id:'T-1',title:'repair',phase:'working',stage:'working',lastFailure:'gen2 Version locator failure'}];
  await refreshed(page);
  await expect(box.locator('.project-failure')).toHaveCount(0);
  await expect(box).toContainText('Last recorded failure (historical');
  world.cards=[{id:'T-1',title:'repair',phase:'verified',stage:'verified',lastFailure:'gen2 Version locator failure'}];
  await refreshed(page);
  await expect(box.locator('.project-failure')).toHaveCount(0);
  await expect(box).toContainText('Historical, per task. Not project acceptance.');
});

test('full diagnostics keep the tail that holds the cause',async ({page})=>{
  const reason='x'.repeat(65000)+'\nFINAL CAUSE: Version locator missing';
  world.cards=[{id:'T-1',title:'big',phase:'waiting',stage:'waiting',waiting:reason,label:'Verification failed'}];
  await open(page);await pick(page,'T-1');
  await expect(page.locator('#project-inspector .project-cause')).toContainText('FINAL CAUSE: Version locator missing');
  const pre=await page.locator('#project-inspector .project-diagnostic').evaluate(e=>e.textContent||'');
  expect(pre.length).toBe(reason.length);expect(pre.endsWith('Version locator missing')).toBe(true);
  await expect(page.locator('#project-inspector')).toContainText('Full diagnostics (65,');
});

test('live registered identity outranks a stale retirement record; unknown inventory is explicit',async ({page})=>{
  world.cards=[{id:'T-1',title:'w',phase:'working',worker:'px-a'}];
  world.sessions=[{name:'px-a',running:true,status:'idle'}];world.expired=['px-a'];
  await open(page);await pick(page,'T-1');
  const box=page.locator('#project-inspector');
  await expect(box).toContainText('registered, running');await expect(box).not.toContainText('retired (Expired)');
  await expect(box.getByRole('button',{name:'Executor terminal'})).toBeVisible();
  world.sessions=[];await page.evaluate('_expiredWorkerInventoryAttemptAt=0;fetchSessions()');
  await expect(box).toContainText('retired (Expired)');await expect(box.getByRole('button',{name:'Executor terminal'})).toHaveCount(0);
  world.expired=[];world.inventory='fail';await page.evaluate('_expiredWorkerInventoryAttemptAt=0');
  await expect(box).toContainText('retired (Expired)');await expect(box).toContainText('inventory stale'); // last measured record, labelled stale
  world.cards=[{id:'T-1',title:'w',phase:'working',worker:'px-b'}]; // never in any inventory
  await expect(box).toContainText('retirement unknown (inventory unavailable');
  world.sessions=[{name:'px-b',running:false,status:'stopped'}];await page.evaluate('fetchSessions()');
  await expect(box).toContainText('registered, not running');await expect(box.getByRole('button',{name:'Executor terminal'})).toHaveCount(0);
});

test('a stopped verified executor remains inspectable while human review is pending',async ({page})=>{
  world.cards=[{id:'T-1',title:'review me',phase:'verified',stage:'verified',worker:'px-review'}];
  world.sessions=[{name:'px-review',running:true,status:'idle'}];
  world.acceptance={state:'awaiting_human',fingerprint:'f'.repeat(64),criteria:[{id:'owner',requirement:'Review output',verifier:{type:'human'},result:{state:'pending_human',evidence:[]}}]};
  await open(page);await pick(page,'T-1');
  const box=page.locator('#project-inspector');
  await expect(box).toContainText('registered, running');
  // The Projects poll itself must refresh the joined worker inventory. This
  // is the real transition into review; requiring a Workers-tab visit or full
  // page reload leaves two contradictory states on one screen.
  world.sessions=[{name:'px-review',running:false,status:'stopped',review_held:true}];
  await refreshed(page);
  await expect(box).toContainText('stopped and retained for human review');
  await expect(box.getByRole('button',{name:'Review executor terminal'})).toBeVisible();
  world.acceptance={...world.acceptance,state:'rejected'};
  await refreshed(page);
  await expect(box).toContainText('stopped and retained for human review');
  await expect(box.getByRole('button',{name:'Review executor terminal'})).toBeVisible();
});

test('a transient outage recovers by itself without navigation or reload',async ({page})=>{
  world.cards=[{id:'T-1',title:'a',phase:'working'}];
  await open(page);
  world.failAll=true;
  await expect(page.locator('#project-error-retry')).toBeVisible();
  expect(await page.evaluate('_projectsFailures') as number).toBeGreaterThanOrEqual(3);
  await expect(page.locator('#project-progress')).toContainText('Stale: last refresh failed');
  world.failAll=false; // the same loop resumes on its own
  await expect(page.locator('#project-error')).toHaveText('');await expect(page.locator('#project-error-retry')).toBeHidden();
  await expect(page.locator('#project-progress')).not.toContainText('Stale');
  expect(await page.evaluate('_projectsFailures')).toBe(0);
});

test('explicit Retry recovers independently of the refresh timer',async ({page})=>{
  await page.addInitScript(()=>{(window as any)._PROJECT_REFRESH_MS=600000;(window as any)._PROJECT_REFRESH_CAP_MS=600000;});
  world.cards=[{id:'T-1',title:'a',phase:'working'}];
  await open(page);
  world.failAll=true;
  for(let i=0;i<3;i++) await page.evaluate('_projectsLoad()'); // the same function the timer calls; the timer is far away
  await expect(page.locator('#project-error-retry')).toBeVisible();
  world.failAll=false;
  await page.locator('#project-error-retry').click();
  await expect(page.locator('#project-error')).toHaveText('');await expect(page.locator('#project-error-retry')).toBeHidden();
  await expect(page.locator('#project-progress')).not.toContainText('Stale');
});

test('choosing another project resets the failure budget',async ({page})=>{
  world.cards=[{id:'T-1',title:'a',phase:'working'}];
  await open(page);
  world.failAll=true;
  await expect(page.locator('#project-error-retry')).toBeVisible();
  expect(await page.evaluate("_projectChoose('py'),_projectsFailures")).toBe(0);
  await expect(page.locator('#project-error-retry')).toBeHidden();
  world.failAll=false;
  await expect(page.locator('#project-selector')).toHaveValue('py');
  await expect(page.locator('#project-error')).toHaveText('');
});
