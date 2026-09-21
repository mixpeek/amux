// Actual Chromium fixture over shipped UI; no provider call or live receipt mutation.
import { chromium } from 'playwright';
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
const source=readFileSync(process.env.AMUX_PROJECT_UI_SOURCE || 'crates/amux-dashboard/static/app.js','utf8');
const browser=await chromium.launch({headless:true});
try {
  const page=await browser.newPage();
  await page.route('https://project.test/',route=>route.fulfill({contentType:'text/html',body:'<div id="projects-view"></div>'}));
  await page.goto('https://project.test/');
  await page.addScriptTag({content:`
    const API='';let activeView='projects';
    const esc=s=>String(s).replaceAll('&','&amp;').replaceAll('"','&quot;').replaceAll('<','&lt;');const escJs=s=>String(s);
    ${source.slice(source.indexOf('// Project-owned outcomes.'))}
    const storage=new Map([['selected','sample']]);
    _projectStorage=(key,value)=>{if(value!==undefined)storage.set(key,value);return storage.get(key)||'';};
    let requests=[],loseResponse=true,grants=0;
    const data={project:{name:'sample',revision:1,policy:{paused:true,enabled:true,repository:'/repo',coordinator:{provider:'codex',model:'gpt-6-astra'},executor:{provider:'codex',model:'gpt-6-astra'},verify_command:'./verify.sh',max_attempts:2,max_executors:1}},pause_settled:true,usage:{},cards:[],commands:[{id:6,pending:true,text:'Original outcome',attempts:2,retry_revision:0,retry_available:true,waiting_reason:'intake_attempts_exhausted',result:{error:'Provider configuration failure'}}]};
    _projectRequest=async(path,method,body)=>{
      if(method==='POST') {
        requests.push({path,body});
        if(!grants)grants++;
        if(loseResponse){loseResponse=false;throw new Error('Response lost');}
        data.commands[0].retry_available=false;data.commands[0].waiting_reason=null;
        return {ok:true,applied:false,message_id:6};
      }
      if(!path)return {projects:[data.project]};
      return data;
    };
  `});
  await page.evaluate(()=>_projectsLoad());
  assert.match(await page.locator('#project-commands').innerText(),/Intake attempt limit reached/);
  assert.doesNotMatch(await page.locator('#project-commands').innerText(),/clarification/);
  await page.getByRole('button',{name:'Retry intake',exact:true}).click();
  await page.waitForFunction(()=>requests.length===1 && !_projectIntakeRetries.size);
  assert.match(await page.locator('#project-error').innerText(),/Response lost/);
  // The same explicit retry after an uncertain response reuses its durable key.
  await page.getByRole('button',{name:'Retry intake',exact:true}).click();
  await page.waitForFunction(()=>requests.length===2 && !_projectIntakeRetries.size);
  const proof=await page.evaluate(()=>({requests,grants,paused:data.project.policy.paused,receipt:data.commands[0].id}));
  assert.deepEqual(proof.requests[0],proof.requests[1]);
  assert.equal(proof.requests[0].path,'/sample/commands/6/retry');
  assert.equal(proof.requests[0].body.expect_attempts,2);
  assert.equal(proof.requests[0].body.expect_revision,0);
  assert.equal(proof.grants,1);assert.equal(proof.receipt,6);assert.equal(proof.paused,true);
  assert.equal(await page.getByRole('button',{name:'Retry intake',exact:true}).count(),0);
  assert.match(await page.locator('#project-receipt').innerText(),/when resumed/);
  await page.evaluate(()=>{
    data.cards=[{id:'A',title:'Studio output',phase:'waiting',next_action:'Continue after backend verification',acceptance_criteria:[],execution_plan:{waiting_reason:'required_output:B',execution:{stage:'waiting',worker:'executor'}}}];
    _projectRender(data);
  });
  assert.match(await page.locator('#project-cards .project-column h3').innerText(),/^Waiting/);
  assert.match(await page.locator('#project-cards').innerText(),/required output/);
  assert.doesNotMatch(await page.locator('#project-cards').innerText(),/Working now/);
  await page.evaluate(()=>{
    requests=[];loseResponse=true;grants=0;data.project.policy.paused=false;
    Object.assign(data.cards[0],{rev:7,execution_plan:{waiting_reason:'attempts_exhausted',execution:{stage:'waiting',worker:'executor',generation:2,input_hash:'requirements',attempt:2}}});
    _projectRender(data);
  });
  await page.getByRole('button',{name:'Authorize one retry',exact:true}).click();
  await page.waitForFunction(()=>requests.length===1 && !_projectIntakeRetries.size);
  await page.getByRole('button',{name:'Authorize one retry',exact:true}).click();
  await page.waitForFunction(()=>requests.length===2 && !_projectIntakeRetries.size);
  const taskProof=await page.evaluate(()=>({requests,grants}));
  assert.deepEqual(taskProof.requests[0],taskProof.requests[1]);
  assert.equal(taskProof.requests[0].path,'/sample/tasks/A/retry');
  assert.equal(taskProof.requests[0].body.expect_generation,2);assert.equal(taskProof.requests[0].body.expect_revision,7);
  assert.equal(taskProof.grants,1);
  await page.evaluate(()=>{
    Object.assign(data.usage,{measured:true,tokens:300,requested_outcomes:1,cost_measured:false,estimated_cost_usd:null,cost_reason:'execution model rates missing',execution_cost_turns_measured:0,execution_turns_measured:2,executor_unattributed_turns_measured:1,executor_unattributed_tokens:200});
    _projectRender(data);
  });
  assert.match(await page.locator('#project-usage').innerText(),/Cost unknown \(execution model rates missing\)/);
  assert.match(await page.locator('#project-usage').innerText(),/0\/2 execution turns priced/);
  assert.match(await page.locator('#project-usage').innerText(),/1 executor turns outside attempt windows \(200 tokens\)/);
  await page.evaluate(()=>{Object.assign(data.usage,{cost_measured:true,estimated_cost_usd:0,cost_reason:null,execution_cost_turns_measured:2});_projectRender(data);});
  assert.match(await page.locator('#project-usage').innerText(),/\$0\.0000 estimated cost/);
  assert.doesNotMatch(await page.locator('#project-usage').innerText(),/Cost unknown/);
  console.log('project intake retry UI: PASS (original receipt, bounded grant, key reuse, pause preserved, waiting phase, task retry uncertainty key)');
} finally {await browser.close();}
