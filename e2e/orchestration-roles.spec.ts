import {test, expect, allowUnusedRoute} from './fixtures';

const workspace={name:'workspace',dir:'/tmp/project',running:true,status:'idle',lifecycle:'active'};

test.beforeEach(async ({page})=>{
  await page.addInitScript(()=>localStorage.setItem('amux_walkthrough_done','1'));
  // Browser fixtures may use the installed server shell. No mutating request
  // reaches it; the launch route below is an explicit request/response fixture.
  await page.route('**/api/**',r=>r.request().method()==='GET'?r.fallback():r.fulfill({status:409,json:{error:'read-only browser fixture'}}));
  allowUnusedRoute(page,'**/api/**'); // Other routes may handle every request.
});

test('Board has no launch surface and no saved tab state recreates an Orchestrations button',async ({page})=>{
  await page.addInitScript(()=>{
    localStorage.setItem('amux_tab_order',JSON.stringify(['orchestrations','sessions','board']));
    localStorage.setItem('amux_hidden_tabs',JSON.stringify([]));
  });
  await page.route('**/api/sessions',r=>r.fulfill({json:[workspace]}));
  await page.goto('/');
  await page.locator('#tab-board').click();
  await expect(page.locator('#board-view h1')).toContainText('Legacy');
  await expect(page.locator('#board-launch-bar,#launch-input,#launch-btn,.launch-header')).toHaveCount(0);
  await expect(page.locator('#tab-orchestrations')).toHaveCount(0);
  // The global customizer only: the worker peek has its own .tab-customize-btn.
  await page.locator('.tab-customize-wrap .tab-customize-btn').click();
  await expect(page.locator('#tab-customizer-menu [data-tab-id="orchestrations"]')).toHaveCount(0);
  expect(await page.evaluate('_saveTabOrder(),localStorage.getItem("amux_tab_order")')).not.toContain('orchestrations');
  await page.reload();
  await expect(page.locator('#tab-orchestrations')).toHaveCount(0);
  await page.locator('#tab-board').click();
  await page.getByRole('button',{name:'Projects',exact:true}).first().click();
  await expect(page.locator('#projects-view')).toBeVisible();
});

test('orchestration shows coordinator and child models plus the coordinator own work',async ({page})=>{
  const workers=[{name:'coordinator',orchestrator:true,role:'orchestrator',lifecycle:'active',running:true,profile:{provider:'codex',model:'gpt-5'}},
    {name:'child',ephemeral:true,ephemeral_parent:'coordinator',lifecycle:'active',running:true,profile:{provider:'claude',model:'haiku'},worktree_active:true,branch:'amux/fanout/child'}];
  await page.route('**/api/sessions',r=>r.fulfill({json:workers}));
  await page.route('**/api/board/orchestrations',r=>r.fulfill({json:{measured:true,n_considered:3,workers,ephemeral_workers:['child'],cards:[
    {id:'E',title:'Release orchestration',type:'epic',status:'doing',session:'coordinator',execution_terminal:false},
    {id:'C',title:'Repair parser',type:'code',status:'doing',session:'child',epic:'E',execution_terminal:false},
    {id:'O',title:'Resolve the release contract',type:'investigation',status:'todo',session:'coordinator',execution_terminal:false},
  ]}}));
  await page.goto('/');
  await page.evaluate(()=>switchView('orchestrations'));
  const epic=page.locator('[data-orch-id="orchestrator:coordinator"]');
  await expect(epic.locator('[data-orch-worker="coordinator"]')).toContainText('codex · gpt-5');
  await expect(epic).toContainText('0/2');
  await expect(epic.locator('[data-orch-worker="child"]')).toContainText('claude · haiku');
  await epic.locator('[data-orch-worker="coordinator"] .orch-tasks-toggle').click();
  await expect(epic).toContainText('Resolve the release contract');
  await expect(epic).toContainText('amux/fanout/child');
  await expect(page.locator('[data-orch-id="worker:coordinator"]')).toHaveCount(0);
  expect(await page.evaluate(()=>document.documentElement.scrollWidth-innerWidth)).toBeLessThanOrEqual(1);
  await page.screenshot({path:test.info().outputPath('orchestration-role-tree.png'),fullPage:true});
});
