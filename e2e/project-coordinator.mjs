// Focused browser fixture: execute the shipped project UI, never a live provider.
import { chromium } from 'playwright';
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
const source = readFileSync(process.env.AMUX_PROJECT_UI_SOURCE || 'crates/amux-dashboard/static/app.js','utf8');
const projectCode = source.slice(source.indexOf('// Project-owned outcomes.'));
const browser = await chromium.launch({headless:true});
try {
  const page = await browser.newPage();
  await page.setContent('<div id="projects-view"></div>');
  await page.addScriptTag({content: `
    const API=''; let activeView='projects';
    const esc=s=>String(s).replaceAll('&','&amp;').replaceAll('"','&quot;').replaceAll('<','&lt;');
    const escJs=s=>String(s); const storage=new Map();
    let saved=null;let saves=[];
    ${projectCode}
    _projectStorage=(key,value)=>{if(value!==undefined)storage.set(key,value);return storage.get(key)||'';};
    _projectRequest=async(path,method,body)=>{
      if(method==='PUT'){saved={name:'sample',revision:(saved?.revision||0)+1,policy:body.policy};saves.push(body);return {};}
      if(!path)return {projects:saved?[saved]:[]};
      return {project:saved,usage:{},commands:[],cards:[]};
    };
    _projectRender=()=>{};
  `});
  await page.evaluate(()=>_projectsLoad());
  await page.locator('#project-name').fill('sample');
  await page.locator('#project-repository').fill('/tmp/repository');
  await page.locator('#project-coordinator-provider').selectOption('codex');
  await page.locator('#project-coordinator').fill('gpt-6-astra');
  assert.equal(await page.locator('#project-coordinator-models option[value="gpt-6-astra"]').count(),1);
  await page.locator('#project-provider').selectOption('claude');
  await page.locator('#project-executor').fill('sonnet');
  await page.locator('#project-verify').fill('./verify.sh');
  await page.evaluate(async()=>{await _projectSave();while(_projectsLoading)await new Promise(r=>setTimeout(r,10));});
  await page.locator('#project-command').fill('Keep this unsent outcome');
  await page.evaluate(()=>_projectDraft());
  // Same reload path as a fresh page: discard rendered state and load durable policy.
  await page.evaluate(async()=>{_projectsStop();_projectsData=null;document.getElementById('projects-view').innerHTML='';await _projectsLoad();});
  assert.equal(await page.locator('#project-coordinator-provider').inputValue(),'codex');
  assert.equal(await page.locator('#project-coordinator').inputValue(),'gpt-6-astra');
  assert.equal(await page.locator('#project-provider').inputValue(),'claude');
  assert.equal(await page.locator('#project-executor').inputValue(),'sonnet');
  assert.equal(await page.locator('#project-command').inputValue(),'Keep this unsent outcome');
  await page.locator('.project-settings').filter({hasText:'Execution settings'}).evaluate(el=>el.open=true);
  await page.locator('#project-coordinator-provider').selectOption('claude');
  await page.locator('#project-coordinator').fill('haiku');
  await page.locator('#project-provider').selectOption('codex');
  await page.locator('#project-executor').fill('gpt-6-astra');
  await page.evaluate(()=>_projectSave());
  const policy=await page.evaluate(()=>saves.at(-1).policy);
  assert.deepEqual(policy.coordinator,{provider:'claude',model:'haiku'});
  assert.deepEqual(policy.executor,{provider:'codex',model:'gpt-6-astra'});
  console.log('project coordinator UI: PASS (independent profiles, Astra suggestion, save/reload, unsent draft)');
} finally { await browser.close(); }
