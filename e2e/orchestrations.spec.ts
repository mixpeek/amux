import { test, expect } from './fixtures';

test.beforeEach(async({page})=>{
  await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
});

// Helper: create a board card via the API.
async function createCard(
  request: any,
  auth: Record<string, string>,
  data: Record<string, unknown>,
) {
  const res = await request.post('/api/board', { headers: auth, data });
  expect(res.ok(), `create card failed: ${res.status()}`).toBeTruthy();
  return res.json();
}

// Helper: link a child card to an epic via PATCH.
async function linkChild(
  request: any,
  auth: Record<string, string>,
  childId: string,
  epicId: string,
) {
  const res = await request.patch(`/api/board/${childId}`, {
    headers: auth,
    data: { epic: epicId },
  });
  expect(res.ok(), `link child failed: ${res.status()}`).toBeTruthy();
}

  test('epic card detail shows subtasks', async ({ page, request }) => {
    await page.goto('/');
    const token = await page.evaluate(() => (window as any)._AMUX_AUTH_TOKEN);
    const auth = {
      Authorization: `Bearer ${token}`,
      'Content-Type': 'application/json',
      'X-Amux-Session': 'e2e-detail-test',
    };

    const epicTitle = `e2e-detail-epic-${Date.now()}`;
    const epic = await createCard(request, auth, {
      title: epicTitle,
      type: 'epic',
      status: 'doing',
    });

    const childNames = ['Cache invalidation', 'Rate limiter', 'Circuit breaker'];
    for (const title of childNames) {
      const child = await createCard(request, auth, { title, status: 'todo' });
      await linkChild(request, auth, child.id, epic.id);
    }

    // Open the card detail by navigating to the board view and using the
    // deeplink format #issue=<id>. Navigate to / first to get the app shell
    // loaded, then change the hash.
    await page.evaluate((id) => {
      location.hash = '#issue=' + encodeURIComponent(id);
    }, epic.id);

    // Wait for the board detail overlay to appear. The epic title is in an
    // input/textarea (#bd-title) so we check its value, not textContent.
    const titleInput = page.locator('#board-detail-overlay #bd-title');
    await expect(titleInput).toHaveValue(epicTitle, { timeout: 10_000 });

    // The subtasks section renders child titles as text (not inputs).
    for (const title of childNames) {
      await expect(page.locator('#board-detail-overlay')).toContainText(title, { timeout: 5_000 });
    }
  });


// No live worker launches: these fixtures exercise the deployed view/handlers.
const workers = [
  {name:'parent',role:'orchestrator',provider:'codex',profile:{provider:'codex',model:'gpt-5'},lifecycle:'active',running:true},
  {name:'child',ephemeral:true,ephemeral_parent:'parent',lifecycle:'active',running:true,status:'active',task_board_id:'B',runtime_board:{measured:true,status:'linked',runtime_status:'active',card_id:'B'},profile:{provider:'claude',model:'haiku'},worktree_active:true,branch:'amux/fanout/child',worktree_integration:{status:'requires_work',detail:'Validation failed: fix on this worker'}},
  {name:'second',ephemeral:true,ephemeral_parent:'parent',lifecycle:'active',running:false,profile:{provider:'gemini',model:'gemini-2.5-flash'}},
  {name:'orphan',ephemeral:true,lifecycle:'active',running:false},
  {name:'paused-child',ephemeral:true,ephemeral_parent:'paused-parent',lifecycle:'paused',running:false},
  {name:'paused-parent',role:'orchestrator',lifecycle:'paused',running:false},
  {name:'retired',ephemeral:true,lifecycle:'expired',running:false},
  {name:'archived',ephemeral:true,lifecycle:'archived',running:false},
  {name:'pending-coordinator',orchestrator:true,role:'orchestrator',lifecycle:'active',running:false},
];
const cards = [
  {id:'E',title:'Customer reliability epic',type:'epic',status:'doing',session:'parent',execution_terminal:false},
  {id:'A',title:'Assignment implementation',type:'code',status:'done',session:'child',epic:'E',execution_terminal:false},
  {id:'B',title:'Follow-up without an epic link',type:'code',status:'doing',session:'child',execution_terminal:false},
  {id:'C',title:'Independent validation',type:'code',status:'verified',session:'second',epic:'E',execution_terminal:true},
  {id:'CHILD-EPIC',title:'Internal child decomposition',type:'epic',status:'todo',session:'child',execution_terminal:false},
  {id:'D',title:'Nested child task',type:'chore',status:'done',session:'child',epic:'CHILD-EPIC',execution_terminal:true},
  {id:'U',title:'Ordinary non-orchestration epic',type:'epic',status:'doing',session:'ordinary',execution_terminal:false},
  {id:'U2',title:'Unrelated parent board work',type:'code',status:'doing',session:'parent',execution_terminal:false},
];
const snapshot = {measured:true,n_considered:20000,cards,workers};

test.describe('global orchestration groups',()=>{
  test('expired section uses lifecycle inventory for retired project executors',async({page})=>{
    await page.route('**/api/sessions',r=>r.fulfill({json:[]}));
    await page.route('**/api/board/orchestrations',r=>r.fulfill({json:{measured:true,n_considered:1,cards:[],workers:[
      {name:'px-acceptance-a1b2c3d4e5',lifecycle:'expired',running:false,ephemeral:true,ephemeral_parent:'project-coordinator'}
    ]}}));
    await page.goto('/');
    await page.evaluate(()=>eval(`
      sessions = [];
      boardItems = [
        {id:'PU-1',title:'Retained project evidence',type:'code',status:'verified',session:'px-acceptance-a1b2c3d4e5',updated:99}
      ];
      expiredExpanded = true;
      _expiredWorkerInventory = new Map();
      _expiredWorkerInventoryAt = 0;
      _expiredWorkerInventoryAttemptAt = 0;
      _expiredWorkerInventoryError = null;
      _renderExpiredSection();
    `));
    await expect.poll(()=>page.evaluate(()=>eval(`_expiredWorkerInventory.size`))).toBe(1);
    const section = page.locator('#expired-section');
    await expect(section).toContainText('1 expired');
    await expect(section).toContainText('Retained project evidence');
    await expect(section).toContainText('project-coordinator');
    const childHtml = await page.evaluate(()=>eval(`_bdRenderFanoutChildren({children:[{id:'PU-1',title:'Retained project evidence',status:'verified',session:'px-acceptance-a1b2c3d4e5'}]})`));
    expect(childHtml).toContain('expired');
    expect(childHtml).not.toContain('Start</button>');
    await page.evaluate(()=>eval(`
      sessions = [{name:'px-acceptance-a1b2c3d4e5',ephemeral:true,lifecycle:'active',running:true,status:'idle'}];
      _renderExpiredSection();
    `));
    await expect(section).toHaveText('');
    const unknownHtml = await page.evaluate(()=>eval(`
      _expiredWorkerInventory = new Map();
      _bdRenderFanoutChildren({children:[{id:'PU-2',title:'Provisioning executor',status:'doing',session:'px-acceptance-new'}]})
    `));
    expect(unknownHtml).not.toContain('Retired project executor');
    expect(unknownHtml).toContain('Start</button>');
  });

  test('expired inventory failures are bounded and do not clear stale measured data',async({page})=>{
    let attempts = 0;
    let countAttempts = false;
    await page.route('**/api/sessions',r=>r.fulfill({json:[]}));
    await page.route('**/api/board/orchestrations',r=>{ if(countAttempts) attempts++; r.fulfill({status:503,json:{measured:false,error:'fixture'}}); });
    await page.goto('/');
    countAttempts = true;
    await page.evaluate(()=>eval(`
      _expiredWorkerInventory = new Map([['retained-worker',{name:'retained-worker',lifecycle:'expired'}]]);
      _expiredWorkerInventoryAt = 1;
      _expiredWorkerInventoryAttemptAt = 0;
      _expiredWorkerInventoryLoading = false;
      _expiredWorkerInventoryError = null;
      sessions = [];
      boardItems = [{id:'OLD-1',title:'Stale retained worker',type:'code',status:'verified',session:'retained-worker',updated:2}];
      expiredExpanded = true;
      _refreshExpiredWorkerInventory();
    `));
    await expect.poll(()=>attempts).toBe(1);
    await expect(page.locator('#expired-section')).toContainText('stale inventory');
    await expect(page.locator('#expired-section')).toContainText('Stale retained worker');
    await page.evaluate(()=>eval(`_refreshExpiredWorkerInventory()`));
    await expect.poll(()=>attempts).toBe(1);
    const state = await page.evaluate(()=>eval(`({size:_expiredWorkerInventory.size,error:_expiredWorkerInventoryError,attempt:_expiredWorkerInventoryAttemptAt,measured:_expiredWorkerInventoryAt})`));
    expect(state.size).toBe(1);
    expect(String(state.error)).toContain('http_503');
    expect(state.attempt).toBeGreaterThan(0);
    expect(state.measured).toBe(1);
  });

  test('shows each coordinator and fan-out once, with task detail underneath',async({page})=>{
    await page.route('**/api/sessions',r=>r.fulfill({json:workers}));
    await page.route('**/api/board/orchestrations',r=>r.fulfill({json:snapshot}));
    const fullHistory:string[]=[];
    page.on('request',r=>{if(r.url().includes('/api/board?all=1&slim=0')) fullHistory.push(r.url());});
    await page.goto('/');await page.locator('#tab-orchestrations').click();
    await expect(page.locator('#orch-list .orch-node')).toHaveCount(6);
    const group=page.locator('[data-orch-id="orchestrator:parent"]');
    await expect(group.locator('[data-orch-worker]')).toHaveCount(3);
    await expect(group).toContainText('2 fan-outs · 2/4 terminal');
    await expect(group.locator('[data-orch-worker="parent"]')).toContainText('gpt-5');
    await expect(group.locator('[data-orch-worker="child"]')).toContainText('haiku');
    await expect(group.locator('[data-orch-worker="second"]')).toContainText('gemini-2.5-flash');
    await expect(group.locator('.orch-active-task')).toContainText('Working now');
    await expect(group.locator('.orch-active-task')).toContainText('Follow-up without an epic link');
    await expect(group).toContainText('amux/fanout/child');await expect(group).toContainText('requires work');
    await expect(page.locator('#orch-list')).not.toContainText('Ordinary non-orchestration');
    await expect(page.locator('#orch-list')).not.toContainText('Unrelated parent board work');
    await expect(page.locator('[data-orch-id="CHILD-EPIC"]')).toHaveCount(0);
    await expect(group).not.toContainText('Assignment implementation');
    await group.locator('[data-orch-worker="child"] .orch-tasks-toggle').click();
    await expect(group).toContainText('Assignment implementation');
    await expect(group).toContainText('Nested child task');
    await expect(group.locator('[data-orch-worker="child"]')).toHaveCount(1);
    await group.locator('.orch-node-header').click();await expect(group.locator('[data-orch-worker]')).toHaveCount(0);
    await group.locator('.orch-node-header').click();await expect(group).toContainText('Nested child task');
    expect(fullHistory).toEqual([]);
    expect(await page.evaluate(()=>document.documentElement.scrollWidth-innerWidth)).toBeLessThanOrEqual(1);
    await page.screenshot({path:test.info().outputPath('global-orchestration-groups.png'),fullPage:true});
  });
  test('filters use real worker lifecycle and keep unassigned and empty workers',async({page})=>{
    await page.route('**/api/sessions',r=>r.fulfill({json:workers}));
    await page.route('**/api/board/orchestrations',r=>r.fulfill({json:snapshot}));
    await page.goto('/');await page.locator('#tab-orchestrations').click();
    await expect(page.locator('[data-orch-id="worker:orphan"]')).toContainText('No board tasks yet');
    await expect(page.locator('[data-orch-id="orchestrator:pending-coordinator"]')).toContainText('No fan-out workers provisioned yet');
    for(const [filter,name] of [['paused','paused-child'],['archived','archived'],['expired','retired']]){
      await page.locator('.orch-filter-pill[data-filter="'+filter+'"]').click();
      await expect(page.locator('#orch-list .orch-node')).toHaveCount(1);
      await expect(page.locator('#orch-list')).toContainText(name);
    }
    await page.locator('.orch-filter-pill[data-filter="active"]').click();
    await expect(page.locator('#orch-list .orch-node')).toHaveCount(3);
  });
  test('retained task links require measured active work before showing Working now',async({page})=>{
    let currentWorkers:any[]=workers;
    await page.route('**/api/sessions',r=>r.fulfill({json:currentWorkers}));
    await page.route('**/api/board/orchestrations',r=>r.fulfill({json:{...snapshot,workers:currentWorkers}}));
    await page.goto('/');await page.locator('#tab-orchestrations').click();
    const child=page.locator('[data-orch-id="orchestrator:parent"] [data-orch-worker="child"]');
    await child.locator('.orch-tasks-toggle').click();
    const cases=[
      {status:'active',running:true,lifecycle:'active',truth:'linked',live:true},
      {status:'idle',running:true,lifecycle:'active',truth:'runtime-not-active',live:false},
      {status:'waiting',running:true,lifecycle:'active',truth:'runtime-not-active',live:false},
      {status:'active',running:true,lifecycle:'paused',truth:'linked',live:false},
      {status:'active',running:false,lifecycle:'active',truth:'linked',live:false},
      {status:'active',running:true,lifecycle:'expired',truth:'linked',live:false},
      {status:'active',running:true,lifecycle:'active',truth:'unlinked',live:false},
      {status:'active',running:true,lifecycle:'active',truth:'unmeasured',live:false},
    ];
    for(const scenario of cases){
      currentWorkers=workers.map(w=>w.name==='child'?{...w,...scenario,runtime_board:{measured:scenario.truth!=='unmeasured',status:scenario.truth,runtime_status:scenario.status,card_id:'B'}}:w);
      await page.evaluate(()=> (window as any)._orchLoad());
      await expect(child.locator('.orch-active-task strong')).toHaveText(scenario.live?'Working now':'Current task');
      await expect(child.locator('.orch-active-task')).toContainText('Follow-up without an epic link');
      await expect(child.locator('.orch-task.working-now')).toHaveCount(scenario.live?1:0);
      expect(await child.evaluate(el=>el.classList.contains('working-now'))).toBe(scenario.live);
    }
  });
  test('ordinary epics alone do not turn into orchestrations',async({page})=>{
    await page.route('**/api/sessions',r=>r.fulfill({json:[]}));
    await page.route('**/api/board/orchestrations',r=>r.fulfill({json:{measured:true,n_considered:1,cards:[cards[6]],workers:[]}}));
    await page.goto('/');await page.locator('#tab-orchestrations').click();
    await expect(page.locator('#orch-list')).toContainText('No orchestrations or fan-out workers yet');
    await expect(page.locator('#orch-list .orch-node')).toHaveCount(0);
  });
  test('configuration loads before runtime inventory and late status preserves expansion',async({page})=>{
    let release!:()=>void;const gate=new Promise<void>(resolve=>{release=resolve;});
    await page.route('**/api/sessions',async r=>{await gate;await r.fulfill({json:workers.map(w=>({...w,status:'idle'}))});});
    await page.route('**/api/board/orchestrations',r=>r.fulfill({json:{...snapshot,workers:workers.map(w=>({...w,running:null,status:undefined}))}}));
    try {
      await page.goto('/');await page.locator('#tab-orchestrations').click();
      const child=page.locator('[data-orch-id="orchestrator:parent"] [data-orch-worker="child"]');
      await expect(child).toContainText('status pending',{timeout:2500});
      await child.locator('.orch-tasks-toggle').click();await expect(child).toContainText('Assignment implementation');
      release();await expect(child).toContainText('idle');
      await expect(child).toContainText('Assignment implementation');
      await expect(child).toHaveCount(1);
    } finally { release(); }
  });
  test('failed measurement stays explicit and retry recovers',async({page})=>{
    let attempts=0;
    await page.route('**/api/sessions',r=>r.fulfill({json:[]}));
    await page.route('**/api/board/orchestrations',r=>++attempts===1?r.fulfill({status:503,json:{measured:false}}):r.fulfill({json:{measured:true,n_considered:0,cards:[],workers:[]}}));
    await page.goto('/');await page.locator('#tab-orchestrations').click();
    await expect(page.locator('#orch-list [role="alert"]')).toContainText('Could not load');
    await page.locator('#orch-list').getByRole('button',{name:'Retry'}).click();
    await expect(page.locator('#orch-list')).toContainText('No orchestrations or fan-out workers yet');
    expect(attempts).toBe(2);
  });
});
