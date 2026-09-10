import { test, expect } from '../fixtures';
import type { Page } from '@playwright/test';
import { boot, auth, checkpoint, getSessionsResilient } from './evidence';
import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';

async function menu(page: Page, name: string, action: string) {
  const close = page.locator('#peek-overlay.active').getByRole('button', {name:'Close worker',exact:true});
  if (await close.isVisible()) await close.click();
  await page.goto('/'); await page.locator('#tab-sessions').click();
  const card = page.locator(`.card[data-session="${name}"]`).locator('visible=true').first();
  await expect(card).toBeVisible({timeout:30_000});
  await card.locator('.card-menu-btn').click();
  await page.locator(`.card-menu.open [data-worker-action="${action}"]`).click();
}
async function send(page: Page, name: string, text: string) {
  await menu(page,name,'peek-terminal');
  await page.locator('#peek-cmd-input').fill(text);
  const response = page.waitForResponse(r => r.url().endsWith(`/${name}/send`) && r.request().method()==='POST', {timeout:180_000});
  await page.locator('#peek-overlay .send-split-main').click();
  const r = await response; expect(r.ok(),await r.text()).toBe(true);
  expect((await r.json()).submitted).toBe(true);
}

// The observer supplies the initial request and one deliberate criteria amendment.
// All decomposition, code, reviews, task transitions and evidence are worker-produced.
test('LC-COMPLEX-VERIFIED: Sonnet peers decompose linked work, adapt to changed gates, and independently verify every deliverable', async ({page,request},info) => {
  test.setTimeout(3_900_000);
  page.setDefaultTimeout(30_000);
  expect(process.env.AMUX_LIFECYCLE_LAB_ACK).toBe('dedicated-test-instance');
  const run = process.env.AMUX_LIFECYCLE_COMPLEX_RUN || `lc-sonnet-complex-${Date.now()}`;
  const author = `${run}-author`, reviewer = `${run}-reviewer`, group = `${run}-team`;
  const names = [author,reviewer];
  const amendedGate=['Independent peer reproduced the invoice totals and malformed-input diagnostics','Duplicate invoice IDs are rejected, including identical repeated rows','Negative invoice amounts are rejected'];
  const cwd = path.join(process.env.AMUX_LIFECYCLE_LAB_WORKSPACE!,run);
  const observe = process.env.AMUX_LIFECYCLE_COMPLEX_OBSERVE === '1';
  const resumePhase1 = process.env.AMUX_LIFECYCLE_COMPLEX_RESUME_PHASE1 === '1';
  const fresh = !observe && !resumePhase1;
  await mkdir(cwd,{recursive:true});
  if (fresh) await writeFile(path.join(cwd,'invoices.csv'),'id,customer,amount,currency\ni1,Acme,12.50,USD\ni2,Acme,7.50,USD\ni3,Bravo,5.00,USD\n');
  await boot(page); const headers = await auth(page);
  const health = await (await request.get('/health')).json();
  const timeline:any[]=[]; let details:any[]=[]; let messages:any[]=[];
  if (fresh) for (const name of names) {
    const close = page.locator('#peek-overlay.active').getByRole('button', {name:'Close worker',exact:true});
    if (await close.isVisible()) await close.click();
    await page.goto('/'); await page.locator('#tab-sessions').click();
    await page.locator('[onclick*="toggleAddMenu"]').click();
    await page.locator('.card-menu-item',{hasText:'New worker'}).click();
    await page.locator('#create-name').fill(name);
    await page.locator('#create-provider-claude').click();
    await page.locator('#create-model').selectOption('sonnet');
    await page.locator('#create-dir').fill(cwd);
    await expect(page.locator('#create-name')).toHaveValue(name);
    await expect(page.locator('#create-dir')).toHaveValue(cwd);
    await page.locator('#create-overlay').getByRole('button',{name:'Create',exact:true}).click();
    await expect(page.locator('#create-overlay')).not.toHaveClass(/active/,{timeout:60_000});
    await menu(page,name,'groups'); await page.locator('#edit-input').fill(group);
    await page.locator('#edit-overlay').getByRole('button',{name:'Save',exact:true}).click();
    await expect(page.locator('#edit-overlay')).not.toHaveClass(/active/);
    await menu(page,name,'peek-terminal');
    await expect(page.locator('#peek-body')).toContainText(/Sonnet [0-9.]+(?: with [^\n]+)?[·•]/i,{timeout:120_000});
    await checkpoint(page,info,`complex-sonnet-${name}`);
  }
  const roster = await getSessionsResilient(request,headers);
  const workers = await roster.json();
  for (const name of names) { const w=workers.find((w:any)=>w.name===name); expect(w.tags).toContain(group); expect(`${w.model} ${w.flags}`).toMatch(/sonnet/i); }
  const common = `Authorized complex lifecycle acceptance ${run}. Work only in ${cwd}; contact only ${names.join(' and ')}. Use Claude Sonnet and the existing Amux harness. All peer messages MUST use Bash amux send, never Claude native SendMessage. Read your actual captured source-message card and decompose it with amux board decompose <ID> --stdin into an epic and concrete dependent chore children, each with description, priority, next_action and falsifiable acceptance_criteria. Children start Todo/Backlog and must actually be claimed and worked to completion. Preserve ownership and all source messages. Do not invent cards/results, bypass gates, or contact production. All messages name the real task IDs. Register every produced file and actual git commit using amux board artifact; put actual test commands and results in evidence. Read peer tasks and link cross-board dependencies/reviewer where appropriate. Clean up your own non-work FYI captures with individual truthful reasons, but never discard real unfinished deliverables. Preserve independent review evidence. Initialize a git repository here if needed; commit only these scratch deliverables, do not push it. Phase 1 finishes implementation at Done and waits for an intentional criteria amendment before Verified. Use only current gates read from each card. After phase 2 all real child tasks and epics must reach Verified and no run-owned card may remain open. A peer may independently verify another worker's Done task through POST /api/verify/<ID>, but must not claim its ownership. For that endpoint the peer first authors executable criteria through PUT /api/criteria/<ID>, with authored_by {kind:'worker',id:its actual worker ID}, version:1, and criteria [{id:'cri_'+a valid ULID,description:actual condition,verifier:{kind:'command',cmd:actual independent test command,expected_exit:0},required:true}]. GET returns the server-owned version. POST verify accepts {criteria_version:that version,gate_checked:the CURRENT effective Verified checklist}; send X-Amux-Session as your own name. This runs tests and stores independent evidence. Never claim an unchecked checklist. Use the authenticated local Amux origin and existing auth context, do not expose tokens.`;
  if (fresh) {
    await send(page,reviewer,`${common}\nYou are the reviewer. Decompose your source request into two chore children: an independent black-box contract test suite, then a review/evidence receipt depending on that suite. Coordinate with ${author}. Define and execute independent tests for CSV invoice reconciliation, integer cents (avoid floating point), totals USD=2500, Acme=2000, Bravo=500, malformed amount refusal and explicit diagnostics. Test the author's real CLI and output bytes. Ask for changes if any fail and independently rerun after correction. Write independent_check.py and review.json with actual author/epic/child task IDs, your own epic/child IDs, commands, test results and review decision. Send COMPLEX_READY to ${author} only after phase-1 passes. Wait for the author to relay phase-2 criteria. In phase 2 independently test new requirements, include executable negative cases, and perform actual harness verification of the author's Done tasks and epic. The author independently verifies your own tasks. Finish all own work and send COMPLEX_VERIFIED with actual IDs.`);
    await send(page,author,`${common}\nBuild a complete small invoice reconciliation tool from invoices.csv already here. Decompose into at least THREE dependent chore children: (1) strict CSV parsing and integer-cent normalization with tests; (2) CLI generating report.json with total_cents=2500 and customers {Acme:2000,Bravo:500}; (3) a polished responsive report.html and README with usage and actual git commit outputs. The CLI must be python3 reconcile.py <input.csv> <output-directory>, nonzero for invalid input, and write report.json/report.html for valid input. Use standard library only. The dashboard needs a total, customer breakdown, readable errors/empty states, and no horizontal overflow at 375px. Coordinate with ${reviewer}, request real independent review, fix every actual failure, and reach Done in phase 1. Do not transition to Verified until the intentional amendment arrives. Send COMPLEX_PHASE1 with your actual epic and children IDs after the reviewer sends COMPLEX_READY. In phase 2 relay the new gates to the reviewer, implement them, rerun review, and arrange independent harness verification of all your children and epic. Independently verify the reviewer's actual review deliverables too. Write completion.json with author_epic_id,author_task_ids,reviewer_epic_id,reviewer_task_ids,commit and final_gate_criteria. Then send COMPLEX_VERIFIED to the reviewer.`);
  }
  if (resumePhase1) for (const name of names) await send(page,name,'continue');
  const read = async () => {
    details=[];messages=[];
    for (const name of names) {
      const r=await request.get(`/api/board?session=${name}&done_limit=0`,{headers}); if (!r.ok()) return false;
      for (const card of await r.json()) {
        const d=await request.get(`/api/board/${card.id}`,{headers}); if (!d.ok()) return false; details.push(await d.json());
      }
      const h=await request.get(`/api/history?session=${name}&limit=250`,{headers}); if (!h.ok()) return false; messages.push(...await h.json());
    }
    timeline.push({at:new Date().toISOString(),cards:details.map(c=>({id:c.id,status:c.status,epic:c.epic,type:c.type,verification:c.verification}))});
    return true;
  };
  try {
    if (!observe) {
      await expect.poll(async()=>await read() && messages.some(m=>m.origin===author && m.session===reviewer && m.text.startsWith('COMPLEX_PHASE1')),
        {timeout:1_800_000,intervals:[10000,20000,30000],message:'Sonnet workers must build and review the decomposed phase-1 deliverables'}).toBe(true);
      const epics=details.filter(c=>c.type==='epic'); expect(epics).toHaveLength(2);
      const authorEpic=epics.find(c=>c.session===author); expect(authorEpic.children.length).toBeGreaterThanOrEqual(3);
      expect(authorEpic.messages.length).toBeGreaterThan(0);
      const gate=amendedGate;
      await page.goto('/'); await page.locator('#tab-board').click(); await page.locator('#bv-status').click();
      const edit=page.locator('[onclick*="editStatusGate(\'verified\')"]');
      await edit.click(); await page.locator('#_gate-edit-ta').fill(gate.join('\n')); await page.locator('#_gate-edit-save').click();
      await expect(page.locator('#_gate-edit-ta')).toHaveCount(0);
      await checkpoint(page,info,'changed-verified-gate');
      await send(page,author,`Intentional phase-2 criteria amendment for ${run}, epic ${authorEpic.id}. The Verified column checklist has changed; read the current effective gate and relay it to ${reviewer}. Add actual duplicate invoice ID rejection (including identical repeated rows) and negative amount rejection. Reopen the affected Done work honestly if needed, implement and test, then obtain independent peer review and harness verification through the CURRENT checklist. Produce completion.json and take all real tasks and both epics to Verified. Your tasks remain your own; the peer may verify through /api/verify. This is the only observer amendment; no completion/evidence will be supplied by the observer.`);
    }
    await expect.poll(async()=>await read() && details.filter(c=>c.type==='epic').length>=2
      && details.every(c=>['verified','discarded','cancelled'].includes(c.status))
      && names.every(name=>details.some(c=>c.session===name&&c.status==='verified')),
      {timeout:1_800_000,intervals:[10000,20000,30000],message:'Every real Sonnet deliverable must reach Verified under changed criteria'}).toBe(true);
    const file=async(name:string)=>{
      // Workers choose output directories. Follow their actual registered files,
      // preserving the same byte assertions instead of requiring a root-level copy.
      const artifact=details.flatMap(c=>c.artifacts||[]).filter(a=>a.availability?.exists
        && path.basename(a.resolved_ref||a.ref)===name).sort((a,b)=>b.created_at-a.created_at)[0];
      const target=artifact?.resolved_ref||path.join(cwd,name);
      expect(path.relative(cwd,target).startsWith('..'), 'output belongs to the worker workspace').toBe(false);
      const r=await request.get(`/api/fs/read?path=${encodeURIComponent(target)}`,{headers});
      expect(r.ok(),await r.text()).toBe(true);return (await r.json()).content;
    };
    const receipt=JSON.parse(await file('completion.json'));
    const ids=[receipt.author_epic_id,...receipt.author_task_ids,receipt.reviewer_epic_id,...receipt.reviewer_task_ids];
    expect(new Set(ids).size).toBeGreaterThanOrEqual(7);
    for (const id of ids) expect(details.some(c=>c.id===id&&c.status==='verified')).toBe(true);
    for (const card of details.filter(c=>c.status==='verified')) {expect(card.status).toBe('verified');expect(card.verification.state).toBe('current');
      expect(card.verification.method).toBe('independent_harness');
      expect(card.verification.actor).toBe(card.session===author?reviewer:author);
      expect([...card.verification.gate_snapshot].sort()).toEqual([...amendedGate].sort());
      expect(card.messages.length).toBeGreaterThan(0); expect(String(card.evidence||card.last_result)).not.toBe('');
      expect(card.artifacts.length+card.asset_links.length).toBeGreaterThan(0);
    }
    expect(details.some(c=>c.depends_on.length>0)).toBe(true);
    const output=JSON.parse(await file('report.json'));expect(output.total_cents).toBe(2500);expect(output.customers).toEqual({Acme:2000,Bravo:500});
    await info.attach('worker-completion',{body:JSON.stringify(receipt),contentType:'application/json'});
    await info.attach('independent-review',{body:await file('review.json'),contentType:'application/json'});
    const graph=await request.get('/api/graph/board/verify',{headers});expect(graph.ok()).toBe(true);
    const graphResult=await graph.json();expect(graphResult.measured).toBe(true);expect(graphResult.verification.valid,JSON.stringify(graphResult)).toBe(true);
    await info.attach('board-graph',{body:JSON.stringify(graphResult),contentType:'application/json'});
    const html=await file('report.html');const outputPage=await page.context().newPage();
    for(const size of [{width:1280,height:800},{width:375,height:812}]) {
      await outputPage.setViewportSize(size);await outputPage.setContent(html);await expect(outputPage.locator('body')).toContainText('Acme');await checkpoint(outputPage,info,`produced-report-${size.width}`);
      await page.setViewportSize(size);
      await page.goto('/#issue='+receipt.author_epic_id);await expect(page.locator('#bd-key')).toHaveText(receipt.author_epic_id);await checkpoint(page,info,`verified-epic-${size.width}`);
      for(const name of names) {
        await menu(page,name,'peek-terminal');
        await page.getByRole('button',{name:'Filter messages',exact:true}).click();await page.locator('[name="peek-filter-source"][value="session"]').check();
        await page.getByRole('dialog',{name:'Filter worker messages'}).getByRole('button',{name:'Done',exact:true}).click();
        await page.getByRole('button',{name:'Find in terminal',exact:true}).click();await page.locator('#peek-search').fill('COMPLEX_VERIFIED');
        await expect(page.locator('#peek-body .peek-highlight').first()).toBeVisible({timeout:30_000});await checkpoint(page,info,`complex-terminal-${name}-${size.width}`);
      }
    }
    await outputPage.close();
  } finally {
    await info.attach('complex-proof',{body:JSON.stringify({run,names,group,cwd,health,observe,resumePhase1,observer_actions:'initial requests and one explicit gate amendment; resumed phase 1 sends continue to existing workers; no worker code/evidence/completion writes',timeline,details,messages},null,2),contentType:'application/json'});
  }
});
