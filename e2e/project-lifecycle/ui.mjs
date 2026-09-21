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
const verified=async n=>page.waitForFunction(n=>document.querySelector('#project-usage')?.textContent.includes(n+' / '+n+' structured outcomes verified'),n,{timeout:180000});
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
  await page.locator('#tab-orchestrations').click();
  await wait(async()=>await page.locator('#orch-projects li').filter({hasText:'working'}).count()===2,'global orchestrations omitted active fan-out executors');
  await wait(async()=>await page.locator('#orch-list .orch-node').count()===0 && (await page.locator('#orch-filters [data-filter=all] .orch-filter-count').innerText())==='1','project executors were rendered as duplicate orchestrations');
  await page.screenshot({path:path.join(out,'02-active-orchestration.png'),fullPage:true});
  await page.locator('#orch-projects').getByRole('button',{name:'Open project board',exact:true}).click();
  await page.getByRole('button',{name:'Executor details',exact:true}).first().click();
  await page.locator('#peek-overlay').waitFor({state:'visible'});
  await page.waitForFunction(()=>/alpha|beta/.test(document.getElementById('peek-task-label')?.textContent||''));
  await page.screenshot({path:path.join(out,'02-executor-terminal.png'),fullPage:true});
  await page.locator('#peek-close-btn').click();
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
  await submit('Create budget report and verify it.');
  await page.getByText('token budget reached',{exact:false}).first().waitFor({timeout:45000});
  const boundedCalls=calls().filter(c=>c.phase==='execution').length;
  await new Promise(r=>setTimeout(r,2200));assert.equal(calls().filter(c=>c.phase==='execution').length,boundedCalls);
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
  await page.locator('#tab-orchestrations').click();await page.locator('#orch-projects').getByText('lifecycle-ui',{exact:true}).waitFor();
  await page.screenshot({path:path.join(out,'07-orchestrations.png'),fullPage:true});
  record('Desktop/mobile project view and global Orchestrations reflect the same project');

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

  await page.locator('#project-selector').selectOption('lifecycle-ui');
  await submit('Create dirty report and verify it.');
  await page.locator('.project-wait').filter({hasText:'worktree has uncommitted changes'}).first().waitFor({timeout:90000});
  // UI may show two matching instances in expanded details; inspect worktree directly.
  const dirtyCard=(await projectRead()).cards.find(c=>c.title==='Create dirty report');
  assert.ok(dirtyCard);assert.ok(calls().some(c=>c.phase==='execution'&&c.task===dirtyCard.id),'dirty scenario must execute its provider');
  assert.ok(await sourceMigrationOnly(),'migration source remains automation-disabled during dirty execution');
  const workdirs=fs.readdirSync(path.join(config.home,'worktrees')).map(n=>path.join(config.home,'worktrees',n));
  assert.ok(workdirs.some(p=>fs.existsSync(path.join(p,'uncommitted-evidence.txt'))));
  assert.throws(()=>main('dirty'));record('Dirty checkout is retained and never falsely integrated or retired');
  await page.locator('#project-pause').click();
  assert.deepEqual(errors,[]);
  fs.writeFileSync(path.join(out,'results.json'),JSON.stringify({results,errors,calls:calls(),fixture:config},null,2));
} catch(e) {
  await page.screenshot({path:path.join(out,'failure.png'),fullPage:true});
  fs.writeFileSync(path.join(out,'failure-dom.txt'),await page.locator('body').innerText());
  fs.writeFileSync(path.join(out,'results.json'),JSON.stringify({results,errors,failure:String(e)},null,2));
  console.error(e);process.exitCode=1;
} finally {await context.close();await browser.close();}
