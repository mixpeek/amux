// All product actions use the UI, with a real isolated server and no API mocks.
// Filesystem writes below only inject fixture faults. Reads independently prove Git/process outcomes.
import {chromium} from 'playwright';
import fs from 'node:fs';
import path from 'node:path';
import assert from 'node:assert/strict';
import {execFileSync,spawn} from 'node:child_process';
const config=JSON.parse(fs.readFileSync(process.argv[2],'utf8'));
assert.match(config.home,/^\/private\/tmp\/amux-project-|^\/tmp\/amux-project-/);
const out=path.resolve(process.argv[3] || 'test-results/project-lifecycle');fs.mkdirSync(out,{recursive:true});
const browser=await chromium.launch({headless:true,args:['--ignore-certificate-errors']});
const context=await browser.newContext({ignoreHTTPSErrors:true,viewport:{width:1440,height:1000}});
const page=await context.newPage();const errors=[],results=[];
page.on('pageerror',error=>errors.push(error.message));
page.on('console',msg=>{if(msg.type()==='error') console.log('browser:',msg.text());});
const wait=async(fn,message,ms=150000)=>{const end=Date.now()+ms;while(Date.now()<end){if(await fn())return;await new Promise(r=>setTimeout(r,250));}throw new Error(message);};
const calls=()=>fs.existsSync(path.join(config.home,'fixture-calls.jsonl'))?fs.readFileSync(path.join(config.home,'fixture-calls.jsonl'),'utf8').trim().split('\n').filter(Boolean).map(JSON.parse):[];
const fault=(name,on)=>{const p=path.join(config.home,'hold-'+name);if(on)fs.writeFileSync(p,'test hold');else fs.rmSync(p,{force:true});};
const beat=name=>path.join(config.home,'heartbeat-'+name);
const main=name=>execFileSync('git',['--git-dir',config.remote,'show','main:'+name+'.txt'],{encoding:'utf8'}).trim();
const submit=async text=>{await page.locator('#project-command').fill(text);await page.getByRole('button',{name:'Submit outcome',exact:true}).click();await page.waitForFunction(()=>document.querySelector('#project-command')?.value==='');};
const saveSettings=async()=>{await page.getByRole('button',{name:'Save settings',exact:true}).click();await page.waitForFunction(()=>document.querySelector('.project-settings') && !document.querySelector('.project-settings').open);};
const verified=async n=>page.waitForFunction(n=>document.querySelector('#project-progress')?.textContent.includes(n+' / '+n+' structured outcomes verified'),n,{timeout:180000});
const retired=async()=>wait(()=>!fs.readdirSync(path.join(config.home,'sessions')).some(n=>n.startsWith('px-')&&n.endsWith('.env')),'verified executors did not retire');
const record=(scenario,extra={})=>{const result={verdict:'PASS',scenario,...extra};results.push(result);console.log(JSON.stringify(result));};
async function create(name,capacity='1'){
  await page.getByRole('button',{name:'+ New project',exact:true}).click();
  await page.locator('#project-name').fill(name);await page.locator('#project-repository').fill(config.repo);
  await page.locator('#project-verify').fill('git diff --check');await page.locator('#project-capacity').selectOption(capacity);
  await page.getByRole('button',{name:'Create project',exact:true}).click();await page.locator('#project-command').waitFor();
}
try {
  await page.goto(config.url,{waitUntil:'domcontentloaded'});
  // Dismiss the existing first-run tour through its actual control.
  await page.getByText('Skip',{exact:true}).click({timeout:3000}).catch(()=>{});
  await page.locator('#tab-projects').click();
  await create('lifecycle-ui','2');
  fault('alpha',true);fault('beta',true);
  const command='Create parallel alpha and beta reports with verified contents.';
  await page.locator('#project-command').fill(command);
  await page.reload({waitUntil:'domcontentloaded'});await page.locator('#tab-projects').click();
  await page.waitForFunction(text=>document.getElementById('project-command')?.value===text,command);
  await submit(command);
  await wait(()=>fs.existsSync(beat('alpha'))&&fs.existsSync(beat('beta')),'parallel executors did not both start');
  assert.equal(calls().filter(c=>c.phase==='execution').length,2);
  await page.screenshot({path:path.join(out,'01-parallel-working.png'),fullPage:true});
  // Same command, new receipt: canonical board and interpretation are reused.
  await submit(command);
  await wait(async()=>await page.locator('.project-card').count()===3,'duplicate board cards');
  assert.equal(calls().filter(c=>c.phase==='intake').length,1);
  // Projects are the only navigation entry for project work; orchestrations are legacy history.
  assert.equal(await page.locator('#tab-orchestrations').count(),0,'global Orchestrations tab must be gone');
  assert.equal(await page.locator('#project-legacy').count(),1,'legacy history stays discoverable from Projects');
  await page.screenshot({path:path.join(out,'02-project-board.png'),fullPage:true});
  // Task inspector: open it, expand a disclosure, and prove a fresh refresh keeps everything as the user left it.
  await page.locator('#project-cards .project-card-select').first().click();
  const inspector=page.locator('#project-inspector');await inspector.getByText('Criteria and evidence',{exact:true}).waitFor();
  const selectedTask=await page.locator('#project-cards .project-selected').getAttribute('data-task');
  const history=inspector.locator('details.project-section').filter({hasText:'Task verification history'});
  await history.locator('summary').click();await history.locator('summary').click();
  assert.equal(await history.evaluate(d=>d.open),false);
  await history.locator('summary').click();assert.equal(await history.evaluate(d=>d.open),true);
  const loadsBefore=await page.evaluate(()=>_projectsToken);
  await page.waitForFunction(t=>_projectsToken>=t+2,loadsBefore,{timeout:15000});
  assert.equal(await history.evaluate(d=>d.open),true,'expanded task details must survive a refresh');
  assert.equal(await page.locator('#project-cards .project-selected').getAttribute('data-task'),selectedTask,'selected task must survive a refresh');
  await page.setViewportSize({width:390,height:844});
  const phaseLoads=await page.evaluate(()=>_projectsToken);await page.waitForFunction(t=>_projectsToken>=t+2,phaseLoads,{timeout:15000});
  assert.equal(await history.evaluate(d=>d.open),true,'expanded task details must survive a refresh on a phone');
  await page.screenshot({path:path.join(out,'02-inspector-phone.png'),fullPage:true});
  await page.setViewportSize({width:1440,height:1000});
  await page.locator('#project-cards .project-card:has(.project-working) .project-card-select, #project-cards .project-card-select').first().click();
  const terminal=inspector.getByRole('button',{name:'Executor terminal',exact:true});
  if(await terminal.count()){
    await terminal.click();
    await page.locator('#peek-overlay').waitFor({state:'visible'});
    await page.waitForFunction(()=>/alpha|beta/.test(document.getElementById('peek-task-label')?.textContent||''));
    await page.screenshot({path:path.join(out,'02-executor-terminal.png'),fullPage:true});
    await page.locator('#peek-close-btn').click();
  }
  fault('alpha',false);fault('beta',false);await verified(1);await retired();
  assert.equal(main('alpha'),'alpha');assert.equal(main('beta'),'beta');
  assert.equal(calls().filter(c=>c.phase==='execution').length,2,'duplicate provider execution');
  assert.equal(fs.readdirSync(path.join(config.home,'worktrees')).length,0);
  record('UI intake, reload draft, duplicate reconciliation, two isolated executors, main verification and retirement',{intakeCalls:1,executionCalls:2});

  fault('pause',true);await submit('Create pause report and verify it.');
  await wait(()=>fs.existsSync(beat('pause')),'pause executor never started');
  await page.locator('#project-pause').click();await page.waitForFunction(()=>document.querySelector('#project-state')?.textContent==='Paused');
  const pausedBeat=fs.readFileSync(beat('pause'),'utf8');await new Promise(r=>setTimeout(r,1800));
  assert.equal(fs.readFileSync(beat('pause'),'utf8'),pausedBeat,'paused provider still writing');
  await page.screenshot({path:path.join(out,'03-paused.png'),fullPage:true});
  fault('pause',false);await page.locator('#project-pause').click();await verified(2);await retired();
  assert.equal(main('pause'),'pause');record('UI pause stops running work; resume completes same task without adding an attempt');

  await submit('Create repair report and verify it.');await verified(3);await retired();
  assert.equal(main('repair'),'repair');
  const repairCalls=calls().filter(c=>c.phase==='execution'&&c.worker===calls().filter(c=>c.phase==='execution').at(-1).worker);assert.deepEqual(repairCalls.map(c=>c.attempt),[1,2]);
  record('Failed artifact check triggers one bounded repair and verified integration');

  fault('restart',true);await submit('Create restart report and verify it.');
  await wait(()=>fs.existsSync(beat('restart')),'restart executor never started');
  await page.locator('#project-command').fill('Unsubmitted draft survives server restart');
  const processCommand=execFileSync('ps',['-p',String(config.pid),'-o','command='],{encoding:'utf8'});
  assert.ok(processCommand.includes(config.binary));process.kill(config.pid,'SIGTERM');
  await wait(()=>{try{return execFileSync('ps',['-p',String(config.pid),'-o','stat='],{encoding:'utf8'}).trim().startsWith('Z')}catch{return true}},'test server did not stop',30000);
  const log=fs.openSync(path.join(out,'restart-server.log'),'a');
  const server=spawn('python3',['e2e/project-lifecycle/serve.py','--binary',config.binary,'--home',config.home,'--port',new URL(config.url).port],{stdio:['ignore',log,log],detached:true});server.unref();fs.closeSync(log);
  await wait(async()=>{try{const response=await context.request.get(config.url+'/health');return response.ok()&&(await response.json()).pid===server.pid}catch{return false}},'test server did not restart',60000);
  await page.reload({waitUntil:'domcontentloaded'});await page.locator('#tab-projects').click();
  await page.waitForFunction(()=>document.getElementById('project-command')?.value==='Unsubmitted draft survives server restart');
  fault('restart',false);await verified(4);await retired();assert.equal(main('restart'),'restart');
  record('Server restart preserves claim, running provider, project state and local draft');

  await page.getByText('Execution settings',{exact:true}).click();
  await page.locator('#project-token-budget').fill('100');await saveSettings();
  const budgetRequest='Create budget report and verify it.';
  const beforeBudgetCalls=calls().length;
  await submit(budgetRequest);
  const budgetReceipt=(await page.locator('#project-receipt').innerText()).match(/^Request (\d+) accepted$/);
  assert.ok(budgetReceipt,'budget request must retain its accepted receipt');
  const budgetRow=page.locator('#project-commands .project-intake').filter({has:page.getByText(budgetRequest,{exact:true})});
  await budgetRow.getByText('Request '+budgetReceipt[1]+' · token budget reached',{exact:true}).waitFor({timeout:45000});
  await budgetRow.getByText('token_budget_reached',{exact:true}).waitFor();
  assert.equal(await page.locator('.project-card').filter({hasText:'Create budget report'}).count(),0,'budget must hold intake before creating a card');
  const boundedCalls=calls().filter(c=>c.phase==='execution').length;
  await new Promise(r=>setTimeout(r,2200));assert.equal(calls().filter(c=>c.phase==='execution').length,boundedCalls);
  assert.equal(calls().length,beforeBudgetCalls,'held budget request must not call intake or execution');
  await page.screenshot({path:path.join(out,'04-budget-wait.png'),fullPage:true});
  await page.getByText('Execution settings',{exact:true}).click();await page.locator('#project-token-budget').fill('');
  await saveSettings();await verified(5);await retired();assert.equal(main('budget'),'budget');
  record('Observed budget stops new claims, explains waiting, resumes after explicit policy edit');

  const beforeQuotaCalls=calls().length;
  await submit('Handle provider quota wait and retain the command.');
  await page.getByText(/provider quota wait/i).first().waitFor({timeout:60000});
  await page.getByText(/resets Sep 23 at 11am/).first().waitFor();
  await wait(()=>calls().length>=beforeQuotaCalls+2,'quota attempts did not settle');
  const beforeQuotaIdle=calls().length;await new Promise(r=>setTimeout(r,2400));assert.equal(calls().length,beforeQuotaIdle);
  record('Provider quota is shown with its reset time, retains the command, and makes no unbounded retries');

  const auth={Authorization:'Bearer '+await page.evaluate(()=>window._AMUX_AUTH_TOKEN)};
  const projectRead=async()=>{const r=await context.request.get(config.url+'/api/projects/lifecycle-ui',{headers:auth});assert.ok(r.ok());return r.json();};
  const malformed='Create malformed report and verify it.';
  const distinct='Create another malformed report and verify it.';
  const parserError='interpretation returned no JSON object';
  const intakeCount=text=>calls().filter(c=>c.phase==='intake'&&c.command===text).length;
  const exhausted=async(text,count)=>{
    await wait(async()=>{
      const receipts=(await projectRead()).commands.filter(c=>c.text===text);
      return receipts.length===count && receipts.every(c=>c.waiting_reason==='intake_attempts_exhausted');
    },'specific request did not exhaust: '+text,60000);
    const receipts=(await projectRead()).commands.filter(c=>c.text===text);
    for(const receipt of receipts){assert.equal(receipt.attempts,receipt.result.waiting_on ? 0 : 2);assert.equal(receipt.attempt_limit,2);assert.equal(receipt.result.error,parserError);assert.equal(receipt.pending,true);}
    const visible=page.locator('#project-commands .project-intake').filter({has:page.locator('p').filter({hasText:new RegExp('^'+text.replace(/[.*+?^${}()|[\]\\]/g,'\\$&')+'$')})});
    const exhaustedLabel=/^Request \d+ · Intake attempt limit reached$/;
    await wait(async()=>{
      if(await visible.count()!==count)return false;
      for(const row of await visible.all()) {
        if(!await row.isVisible() || !exhaustedLabel.test(await row.locator('strong').innerText()))return false;
        if(!(await row.locator('p').allInnerTexts()).includes(parserError))return false;
      }
      return true;
    },'exhausted receipts did not render exact label and parser error: '+text,60000);
    for(const row of await visible.all()) {assert.match(await row.locator('strong').innerText(),exhaustedLabel);assert.ok((await row.locator('p').allInnerTexts()).includes(parserError));}
    return receipts;
  };
  await submit(malformed);await exhausted(malformed,1);assert.equal(intakeCount(malformed),2);
  await submit(distinct);await exhausted(distinct,1);assert.equal(intakeCount(distinct),2);
  const beforeDuplicate=calls().length;
  await submit(malformed);const inherited=await exhausted(malformed,2);
  assert.equal(calls().length,beforeDuplicate,'duplicate failure must not call provider');
  const original=inherited.find(c=>!c.result.waiting_on),duplicate=inherited.find(c=>c.result.waiting_on);
  assert.ok(original&&duplicate);assert.equal(duplicate.result.waiting_on,original.id);assert.equal(duplicate.result.error,original.result.error,'duplicate retains exact parser failure');
  const beforeRecheckIds=(await projectRead()).cards.map(c=>c.id).sort();
  assert.equal(beforeRecheckIds.length,7);
  const later='Recheck alpha report and verify it.';
  await submit(later);
  await wait(async()=>(await projectRead()).commands.some(c=>c.text===later&&!c.pending),'exhausted requests starved later distinct work');
  assert.equal(intakeCount(later),1);
  await wait(async()=>{
    const cards=(await projectRead()).cards;
    assert.deepEqual(cards.map(c=>c.id).sort(),beforeRecheckIds,'recheck must preserve exact card identities');
    return cards.every(c=>c.phase==='verified');
  },'same seven rechecked project cards did not all become Verified',180000);
  await retired();
  assert.deepEqual((await projectRead()).cards.map(c=>c.id).sort(),beforeRecheckIds);
  assert.equal(intakeCount(later),1);
  const beforeIdle=calls().length;await new Promise(r=>setTimeout(r,2400));assert.equal(calls().length,beforeIdle);
  assert.equal(await page.locator('.project-card').count(),7);
  record('Malformed intake retains exact parser errors and finite calls; duplicates inherit failure without calls; later distinct work proceeds',{malformedCalls:intakeCount(malformed),distinctMalformedCalls:intakeCount(distinct),laterCalls:intakeCount(later),duplicateCalls:0});

  await page.screenshot({path:path.join(out,'05-desktop.png'),fullPage:true});
  await page.setViewportSize({width:390,height:844});await page.screenshot({path:path.join(out,'06-mobile.png'),fullPage:true});
  assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth+1),'mobile page overflow');
  assert.ok(await page.locator('.project-card').evaluateAll(cards=>cards.every(c=>{const r=c.getBoundingClientRect();return r.left>=0 && r.right<=innerWidth;})),'mobile cards are clipped');
  await page.setViewportSize({width:1440,height:1000});
  await page.locator('#project-legacy').click();await page.locator('#orchestrations-view h2').filter({hasText:'Legacy orchestration history'}).waitFor();
  assert.equal(await page.locator('#orch-projects').count(),0,'legacy history must not re-project project work as a second authority');
  await page.screenshot({path:path.join(out,'07-legacy-history.png'),fullPage:true});
  record('Desktop/mobile project board fits; old orchestration records stay reachable as explicit legacy history');

  await page.locator('#tab-projects').click();await create('migration-ui');await page.locator('#project-pause').click();
  await page.waitForFunction(()=>document.querySelector('#project-state')?.textContent==='Paused');
  // Configuration is real and persisted; these paused profiles make no provider calls.
  for (const provider of ['codex','gemini','ollama','claude']) {
    await page.getByText('Execution settings',{exact:true}).click();
    await page.locator('#project-provider').selectOption(provider);
    await page.locator('#project-executor').fill(provider==='claude'?'sonnet':'fixture-'+provider);
    await page.locator('#project-coordinator').fill('haiku');
    await saveSettings();
    await page.waitForFunction(provider=>document.getElementById('project-provider')?.value===provider,provider);
    await page.reload({waitUntil:'domcontentloaded'});await page.locator('#tab-projects').click();
    await page.waitForFunction(provider=>document.getElementById('project-provider')?.value===provider,provider);
  }
  record('Independent coordinator/executor profiles persist through UI edits and reload for all four offered executor providers');
  // Seed only the isolated legacy fixture via supported APIs; migration is UI-driven.
  const seededWorker=await context.request.post(config.url+'/api/sessions',{headers:auth,data:{name:'fixture-source',dir:config.repo}});assert.equal(seededWorker.status(),201);
  for(const key of ['board_auto_pickup','board_standing_orders']) {
    const configured=await context.request.patch(config.url+'/api/sessions/fixture-source/config',{headers:auth,data:{[key]:false}});assert.ok(configured.ok());
  }
  const sourceMigrationOnly=async()=>{const r=await context.request.get(config.url+'/api/sessions',{headers:auth});assert.ok(r.ok());const source=(await r.json()).find(s=>s.name==='fixture-source');return source && !source.auto_pickup && !source.standing_orders && source.lifecycle!=='paused' && !source.running;};
  await wait(sourceMigrationOnly,'migration-only source configuration did not settle',30000);
  const seededCard=await context.request.post(config.url+'/api/board',{headers:auth,data:{title:'Legacy raw request',status:'backlog',session:'fixture-source',evidence:'retained fixture evidence'}});assert.ok(seededCard.ok());
  const legacyId=(await seededCard.json()).id;assert.ok(legacyId);
  await page.getByText('Migrate existing boards',{exact:true}).click();await page.locator('#project-migration-workers').fill('fixture-source');
  await page.getByRole('button',{name:'Preview migration',exact:true}).click();await page.locator('#project-migration-apply').waitFor({state:'visible'});
  await page.getByRole('button',{name:'Apply reviewed migration',exact:true}).click();
  await page.locator('[data-task="'+legacyId+'"]').waitFor();
  await page.waitForFunction(()=>document.querySelector('#project-migration-id')?.value.startsWith('project-migration:'));
  await page.getByRole('button',{name:'Roll back unchanged rows',exact:true}).click();
  await page.locator('[data-task="'+legacyId+'"]').waitFor({state:'detached'});
  assert.ok(await sourceMigrationOnly(),'rollback must preserve migration-only automation settings');
  record('UI migration preview/apply/rollback preserves legacy identity and evidence');

  // A gate/environment failure can rerun the same retained report without a model repair.
  await create('verification-retry-ui');
  await page.getByText('Execution settings',{exact:true}).click();
  await page.locator('#project-attempts').fill('1');
  await page.locator('#project-verification-timeout').fill('1');
  await page.locator('#project-verify').fill('if [ -e "$AMUX_HOME/hold-verification-gate" ]; then sleep 3; fi; git diff --check');
  await saveSettings();fault('verification-gate',true);
  await submit('Create reverify report with verified contents.');
  const reverifyRead=async()=>{const response=await context.request.get(config.url+'/api/projects/verification-retry-ui',{headers:auth});assert.ok(response.ok());return response.json();};
  let retained;
  await wait(async()=>{retained=(await reverifyRead()).cards.find(c=>c.execution_plan.execution.report);return retained?.execution_plan.execution.stage==='waiting';},'bounded verification did not retain its failed report');
  assert.match(retained.execution_plan.waiting_reason,/Command timed out after 1 seconds/);
  const rerunCalls=calls().length, retainedExecution=retained.execution_plan.execution;
  await page.getByText('Execution settings',{exact:true}).click();
  await page.locator('#project-verification-timeout').fill('2');await saveSettings();
  await page.reload({waitUntil:'domcontentloaded'});await page.locator('#tab-projects').click();
  await page.waitForFunction(()=>document.getElementById('project-verification-timeout')?.value==='2');
  fault('verification-gate',false);
  await page.locator('[data-task="'+retained.id+'"] .project-card-select').click();await page.locator('#project-inspector').getByRole('button',{name:'Rerun checks',exact:true}).click();
  await wait(async()=>{const c=(await reverifyRead()).cards.find(c=>c.id===retained.id);return c?.phase==='verified';},'retained report did not verify after explicit rerun');
  const reverified=(await reverifyRead()).cards.find(c=>c.id===retained.id).execution_plan.execution;
  assert.equal(reverified.generation,retainedExecution.generation);assert.equal(reverified.attempt,retainedExecution.attempt);
  assert.deepEqual(reverified.report,retainedExecution.report);assert.equal(reverified.verification_retries.length,1);
  assert.match(reverified.verification_retries[0].previous_result.waiting,/Command timed out after 1 seconds/);
  await retired();assert.equal(calls().length,rerunCalls,'rerunning retained checks must not call intake or executor');
  assert.equal(main('reverify'),'reverify');
  record('Explicit verification rerun preserves report/attempt, changes bounded timeout, verifies and retires with zero model calls');

  await page.locator('#project-selector').selectOption('lifecycle-ui');
  await submit('Create dirty report and verify it.');
  await wait(async()=>(await projectRead()).cards.some(c=>c.title==='Create dirty report'&&/worktree has uncommitted changes/.test(c.execution_plan.waiting_reason||'')),'dirty diagnostics did not appear',90000);
  // UI may show two matching instances in expanded details; inspect worktree directly.
  const dirtyCard=(await projectRead()).cards.find(c=>c.title==='Create dirty report');
  assert.ok(dirtyCard);assert.ok(calls().some(c=>c.phase==='execution'&&c.task===dirtyCard.id),'dirty scenario must execute its provider');
  assert.ok(await sourceMigrationOnly(),'migration source remains automation-disabled during dirty execution');
  const workdirs=fs.readdirSync(path.join(config.home,'worktrees')).map(n=>path.join(config.home,'worktrees',n));
  assert.ok(workdirs.some(p=>fs.existsSync(path.join(p,'uncommitted-evidence.txt'))));
  assert.throws(()=>main('dirty'));record('Dirty checkout is retained and never falsely integrated or retired');
  // Owner steering cannot grant itself another attempt, even with Send now.
  let failed;
  await wait(async()=>{failed=(await projectRead()).cards.find(c=>c.id===dirtyCard.id);return failed?.execution_plan.execution.stage==='waiting'&&failed.execution_plan.execution.attempt===2;},'dirty task did not exhaust its authorized attempts');
  await wait(async()=>await page.locator('[data-task="'+dirtyCard.id+'"] .project-wait').innerText()==='Verification failed','failed card short label must be harness-owned');
  const idsBefore=(await projectRead()).cards.map(c=>c.id).sort();
  const callsBefore=calls().length, note='Retained owner note for the same dirty task';
  const packets=()=>fs.readFileSync(path.join(config.home,'fixture-input.jsonl'),'utf8').trim().split('\n').filter(Boolean).map(JSON.parse);
  let noteId,wireNote;
  const ownPackets=()=>packets().filter(p=>p.worker===failed.execution_plan.execution.worker&&p.packet===wireNote);
  const queue=async()=>{const r=await context.request.get(config.url+'/api/sessions/'+encodeURIComponent(failed.execution_plan.execution.worker)+'/steer',{headers:auth});assert.ok(r.ok());return r.json();};
  await page.locator('[data-task="'+dirtyCard.id+'"] .project-card-select').click();
  await page.locator('#project-inspector').getByText(/^Full diagnostics \(/).waitFor();
  assert.match(await page.locator('#project-inspector pre').filter({hasText:'worktree has uncommitted changes'}).first().innerText(),/worktree has uncommitted changes/);
  assert.ok((await page.locator('#project-inspector .project-failure, #project-inspector .project-muted').count())>0);
  await page.locator('#project-inspector').getByRole('button',{name:'Executor terminal',exact:true}).click();
  await page.locator('#peek-cmd-input').fill(note);
  const noteResponse=page.waitForResponse(r=>new URL(r.url()).pathname==='/api/sessions/'+encodeURIComponent(failed.execution_plan.execution.worker)+'/send'&&r.request().method()==='POST');
  await page.locator('#peek-overlay .send-split-main').click();
  const sentResponse=await noteResponse;assert.ok(sentResponse.ok());
  const sentReceipt=await sentResponse.json();noteId=sentReceipt.id;
  assert.equal(typeof noteId,'string');assert.ok(noteId);assert.equal(sentReceipt.submission,'queued');
  assert.equal(sentReceipt.task,dirtyCard.id);
  wireNote=sentResponse.request().postDataJSON().text;
  const envelope=wireNote.match(/^\[(?:0?[1-9]|1[0-2]):[0-5][0-9] (?:AM|PM)\] ([^\r\n]*)$/);
  assert.ok(envelope,'owner send must have exactly one known UI timestamp envelope');
  assert.equal(envelope[1],note,'complete intended body survives UI transport');
  await wait(async()=>{const rows=(await queue()).filter(m=>m.id===noteId);if(rows.length!==1)return false;assert.equal(rows[0].text,wireNote);return rows[0].deliverable===false&&rows[0].blocked_reason?.startsWith('project_active_claim_required:');},'owner note identity was not visibly held');
  await page.locator('#peek-tab-steering').click();
  await page.locator('#peek-steering-list').getByText(wireNote,{exact:true}).locator('..').locator('.steering-held').filter({hasText:'project_active_claim_required'}).waitFor();
  const ownerRow=page.locator('#peek-steering-list').getByText(wireNote,{exact:true}).locator('../..');
  assert.equal(await ownerRow.getByRole('button',{name:'Send now',exact:true}).count(),0,'project note must not offer unsupported Send now');
  await ownerRow.getByText('Automatic next turn',{exact:true}).waitFor();
  assert.equal(await ownerRow.getByRole('button',{name:'✕',exact:true}).isEnabled(),true,'Cancel remains available');
  await new Promise(r=>setTimeout(r,2400));
  assert.equal(ownPackets().length,0,'failed worker consumed held owner input');assert.equal(calls().length,callsBefore,'held note started provider work');
  const heldNote=(await queue()).filter(m=>m.id===noteId);assert.equal(heldNote.length,1);assert.equal(heldNote[0].text,wireNote);assert.equal(heldNote[0].deliverable,false);
  assert.deepEqual((await projectRead()).cards.map(c=>c.id).sort(),idsBefore,'owner input created a board task');
  assert.equal((await projectRead()).cards.find(c=>c.id===dirtyCard.id).execution_plan.execution.attempt,2);
  await page.locator('#peek-close-btn').click();fault('dirty',true);
  await page.locator('[data-task="'+dirtyCard.id+'"] .project-card-select').click();await page.locator('#project-inspector').getByRole('button',{name:'Authorize one retry',exact:true}).click();
  await wait(async()=>{const r=await context.request.get(config.url+'/api/sessions/'+encodeURIComponent(failed.execution_plan.execution.worker)+'/steer?history=1',{headers:auth});assert.ok(r.ok());const delivered=(await r.json()).filter(m=>m.id===noteId);if(!delivered.length)return false;assert.equal(delivered.length,1);assert.equal(delivered[0].text,wireNote);return String(delivered[0].outcome).startsWith('sent');},'authorized retry did not deliver the retained note');
  fault('dirty',false);
  await wait(()=>ownPackets().length===1,'retained owner note not received exactly once');
  await wait(async()=>{const e=(await projectRead()).cards.find(c=>c.id===dirtyCard.id).execution_plan.execution;return e.stage==='waiting'&&e.attempt===3;},'explicit retry did not stop at its new bound');
  assert.equal(calls().length,callsBefore+1,'retry must cause exactly one additional execution and zero intake');
  assert.deepEqual((await projectRead()).cards.map(c=>c.id).sort(),idsBefore);
  assert.equal((await queue()).filter(m=>m.id===noteId).length,0);
  assert.equal(ownPackets().length,1,'same enveloped body must be received exactly once');
  record('Failed task retains owner steering without work; explicit one-retry grant delivers it once within the same task');
  // AAB-11 consolidated workflow: acceptance wording, retired evidence, drafts, settings, empty and error states.
  await page.locator('#project-selector').selectOption('lifecycle-ui');
  await page.locator('#project-acceptance').getByText('Not configured').first().waitFor();
  const retiredCard=(await projectRead()).cards.find(c=>c.phase==='verified'&&(c.execution_plan.execution.retained_assets||[]).length>0);
  assert.ok(retiredCard,'a verified task with retained assets must exist');
  await page.locator('[data-task="'+retiredCard.id+'"] .project-card-select').click();
  await page.locator('#project-inspector').getByText('retired (Expired)').waitFor({timeout:30000});
  await page.locator('#project-inspector .project-report-asset').first().waitFor();
  await page.locator('#project-inspector').getByText('Historical, per task. Not project acceptance.').waitFor();
  assert.equal(await page.locator('#project-inspector').getByRole('button',{name:'Executor terminal',exact:true}).count(),0,'retired executor must not offer a live terminal');
  await page.screenshot({path:path.join(out,'08-retired-assets.png'),fullPage:true});
  // Per-project drafts survive switching.
  await page.locator('#project-command').fill('Draft for lifecycle-ui');
  await page.locator('#project-selector').selectOption('migration-ui');await page.locator('#project-command').waitFor();
  assert.equal(await page.locator('#project-command').inputValue(),'');
  await page.locator('#project-command').fill('Draft for migration-ui');
  await page.locator('#project-selector').selectOption('lifecycle-ui');await page.waitForFunction(()=>document.getElementById('project-command')?.value==='Draft for lifecycle-ui');
  await page.locator('#project-selector').selectOption('migration-ui');await page.waitForFunction(()=>document.getElementById('project-command')?.value==='Draft for migration-ui');
  await page.locator('#project-command').fill('');await page.locator('#project-selector').selectOption('lifecycle-ui');await page.locator('#project-command').fill('');
  // Settings edits persist through refresh until Cancel or Save.
  await page.getByText('Execution settings',{exact:true}).click();
  await page.locator('#project-attempts').fill('4');
  const settingsLoads=await page.evaluate(()=>_projectsToken);await page.waitForFunction(t=>_projectsToken>=t+2,settingsLoads,{timeout:15000});
  assert.equal(await page.locator('#project-attempts').inputValue(),'4','unsaved settings must survive refresh');
  await page.getByRole('button',{name:'Cancel',exact:true}).click();
  assert.notEqual(await page.locator('#project-attempts').inputValue(),'4','Cancel restores saved settings');
  // Empty project and a failed-then-retried read.
  await create('empty-ui');
  await page.locator('#project-cards').getByText('No tasks yet',{exact:false}).waitFor();
  await page.locator('#project-inspector').getByText('Task details appear here',{exact:false}).waitFor();
  await page.screenshot({path:path.join(out,'09-empty-project.png'),fullPage:true});
  await page.route('**/api/projects/empty-ui',route=>route.abort());
  await page.locator('#project-error-retry').waitFor({state:'visible',timeout:90000});
  assert.ok(await page.locator('#project-error').innerText());
  await page.unroute('**/api/projects/empty-ui');
  await page.locator('#project-error-retry').click();
  await page.waitForFunction(()=>!document.getElementById('project-error')?.textContent);
  await page.screenshot({path:path.join(out,'10-error-retry.png'),fullPage:true});
  record('Consolidated project workflow: acceptance Not configured, retired evidence, per-project drafts, settings save/cancel, empty and retry states');
  await page.locator('#project-pause').click();
  assert.deepEqual(errors,[]);
  fs.writeFileSync(path.join(out,'results.json'),JSON.stringify({results,errors,calls:calls(),fixture:config},null,2));
} catch(e) {
  await page.screenshot({path:path.join(out,'failure.png'),fullPage:true});
  fs.writeFileSync(path.join(out,'failure-dom.txt'),await page.locator('body').innerText());
  fs.writeFileSync(path.join(out,'results.json'),JSON.stringify({results,errors,failure:String(e)},null,2));
  console.error(e);process.exitCode=1;
} finally {await context.close();await browser.close();}
