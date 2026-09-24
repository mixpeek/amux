import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
test('project creation retains exact description and stable identity after lost acknowledgement',async()=>{
 const values={name:'sample',repository:'/repo',worktree:'1','coordinator-provider':'codex',coordinator:'gpt-6-luna','coordinator-effort':'low',provider:'codex',executor:'gpt-6-luna','executor-effort':'low','executor-host-access':'0',capacity:'1',verify:'git diff --check','verification-timeout':'600',attempts:'2','token-budget':'','cost-budget':'',contract:'', 'draft-input':'Implement the complete ./spec.md\nPreserve every criterion'};
 const storage=new Map(),requests=[],errors=[],selected=[];let uuid=0;
 const ctx=vm.createContext({document:{getElementById:id=>({value:values[id.replace('project-','')]})},_projectsData:null,crypto:{randomUUID:()=>`id-${++uuid}`},_projectStorage:(key,value)=>{if(value!==undefined)storage.set(key,value);return storage.get(key)||''},_projectSettingsKey:()=> 'settings_',_projectChoose:name=>selected.push(name),_projectError:e=>errors.push(e.message),_projectRequest:async(path,method,body)=>{requests.push(body);if(requests.length===1)throw new Error('lost acknowledgement');}});
 vm.runInContext(source.slice(source.indexOf('async function _projectSave('),source.indexOf('async function _projectPause(')),ctx);
 await ctx._projectSave();assert.equal(errors.length,1);assert.equal(selected.length,0);
 await ctx._projectSave();assert.equal(requests.length,2);
 assert.equal(requests[0].initial_command.text,values['draft-input']);
 assert.equal(requests[0].initial_command.idempotency_key,requests[1].initial_command.idempotency_key);
 assert.equal(uuid,1);assert.deepEqual(selected,['sample']);assert.equal(storage.get('create_sample'),'');
});
test('busy SSE invalidations cannot indefinitely postpone status and queued-message refresh',()=>{
 let timer=null,reads=0,scheduled=0;
 const ctx=vm.createContext({_invSessTimer:null,setTimeout:fn=>{timer=fn;scheduled++;return scheduled},fetchSessions:()=>reads++});
 vm.runInContext(source.slice(source.indexOf('function _scheduleSessionInvalidation('),source.indexOf('async function _fetchSessionsOnce(')),ctx);
 for(let n=0;n<100;n++)ctx._scheduleSessionInvalidation();
 assert.equal(scheduled,1);timer();assert.equal(reads,1);
 ctx._scheduleSessionInvalidation();assert.equal(scheduled,2);timer();assert.equal(reads,2);
 assert.match(source,/if \(key === 'sessions'\) \{\s*_scheduleSessionInvalidation\(\)/);
});
test('project draft text participates in the existing durable settings store',()=>{
 assert.match(source,/const _projectSettingIds=\['draft-input',/);
});

test('model drafting bypasses mutation outbox while real project commands remain durable',()=>{
 const ctx=vm.createContext({location:{origin:'https://amux.example'}});
 vm.runInContext(source.slice(source.indexOf('const _OUTBOX_SKIP ='),source.indexOf('function _outboxAccepted(')),ctx);
 for(const url of ['/api/projects/draft','https://amux.example/api/projects/draft?test=1'])
   assert.equal(ctx._outboxQueueable(url,{method:'POST',body:'{}'}),false);
 assert.equal(ctx._outboxQueueable('/api/projects/example/commands',{method:'POST',body:'{}'}),true);
 assert.equal(ctx._outboxQueueable('/api/projects/example',{method:'PUT',body:'{}'}),true);
});

test('empty project board distinguishes retained intake from a missing outcome',()=>{
 const ctx=vm.createContext({esc:s=>s});
 vm.runInContext(source.slice(source.indexOf('function _projectIntakeState('),source.indexOf('function _projectRender(data)')),ctx);
 assert.match(ctx._projectEmptyTasksHtml({commands:[]}),/Submit an outcome/);
 const queued={commands:[{pending:true}]};
 assert.equal(ctx._projectIntakeState(queued).label,'Preparing board tasks');
 assert.match(ctx._projectEmptyTasksHtml(queued),/outcome is saved/);
 assert.doesNotMatch(ctx._projectEmptyTasksHtml(queued),/Submit an outcome/);
 const held={commands:[{pending:true,waiting_reason:'intake_attempts_exhausted'}]};
 assert.match(ctx._projectIntakeState(held).label,/Intake held/);
 assert.match(ctx._projectEmptyTasksHtml(held),/View request/);
 assert.match(ctx._projectEmptyTasksHtml(held),/Do not resubmit/);
 assert.equal(ctx._projectIntakeState({commands:[{pending:false}]}),null);
});

test('project worker lifecycle uses measured provider status rather than process presence',()=>{
 const worker={name:'demo',lifecycle:'active'};
 const session={name:'demo',running:true,lifecycle:'active',status:'waiting'};
 const ctx=vm.createContext({sessions:[session],_expiredWorkerInventory:new Map()});
 vm.runInContext(source.slice(source.indexOf('function _projectWorkerRuntime('),source.indexOf('async function _projectResumeWorker(')),ctx);
 assert.equal(ctx._projectWorkerRuntime(worker).label,'Needs input');
 session.status='active';assert.equal(ctx._projectWorkerRuntime(worker).label,'Working');
 session.status='idle';assert.equal(ctx._projectWorkerRuntime(worker).label,'Idle');
 session.running=false;assert.equal(ctx._projectWorkerRuntime(worker).label,'Stopped');
 session.lifecycle='paused';assert.equal(ctx._projectWorkerRuntime(worker).label,'Paused');
 ctx.sessions=[];assert.equal(ctx._projectWorkerRuntime({name:'demo',lifecycle:'expired'}).label,'Expired');
});
