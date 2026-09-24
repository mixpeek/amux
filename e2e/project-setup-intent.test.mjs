import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
test('project creation retains exact description and stable identity after lost acknowledgement',async()=>{
 const values={name:'sample',repository:'/repo',worktree:'1','coordinator-provider':'codex',coordinator:'gpt-6-luna','coordinator-effort':'low',provider:'codex',executor:'gpt-6-luna','executor-effort':'low','executor-host-access':'0',capacity:'1',verify:'git diff --check','verification-timeout':'600',attempts:'2','token-budget':'','cost-budget':'',contract:'', 'draft-input':'Implement the complete ./spec.md\nPreserve every criterion'};
 const storage=new Map(),requests=[],errors=[],selected=[];let uuid=0;const errorNode={textContent:'old error'};
 const ctx=vm.createContext({document:{getElementById:id=>id==='project-error'?errorNode:({value:values[id.replace('project-','')]})},console:{info:()=>{}},_projectsData:null,crypto:{randomUUID:()=>`id-${++uuid}`},_projectStorage:(key,value)=>{if(value!==undefined)storage.set(key,value);return storage.get(key)||''},_projectSettingsKey:()=> 'settings_',_projectChoose:name=>selected.push(name),_projectError:e=>errors.push(e.message),_projectRequest:async(path,method,body)=>{requests.push(body);if(requests.length===1)throw new Error('lost acknowledgement');}});
 vm.runInContext(source.slice(source.indexOf('async function _projectSave('),source.indexOf('async function _projectPause(')),ctx);
 await ctx._projectSave();assert.equal(errors.length,1);assert.equal(selected.length,0);
 await ctx._projectSave();assert.equal(requests.length,2);assert.equal(errorNode.textContent,'');
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

test('directory viewer ignores older network and offline cache responses after navigation',async()=>{
 const nodes=new Map(),pending=[],renders=[],cache=[];
 const ctx=vm.createContext({document:{getElementById:id=>{if(!nodes.has(id))nodes.set(id,{innerHTML:'',value:''});return nodes.get(id)}},history:{replaceState:()=>{}},location:{pathname:'/'},_encodeHashPath:s=>s,_updateFilesCwdBtn:()=>{},_filesToolbarCheck:()=>{},esc:s=>s,API:'',_filesShowHidden:false,_renderFilesEntries:(_body,path,data)=>renders.push({path,data}),_autoCacheDirFiles:()=>{},_idb:{setFile:()=>{},getFile:()=>new Promise(resolve=>cache.push(resolve))},fetch:url=>new Promise((resolve,reject)=>pending.push({url,resolve,reject}))});
 vm.runInContext(source.slice(source.indexOf('let _filesLoadGeneration ='),source.indexOf('function _feHighlight(')),ctx);
 const old=ctx.loadFiles('/repo'); const fresh=ctx.loadFiles('/repo/.worktrees/task');
 pending[1].resolve({json:async()=>({entries:['worktree']})});await fresh;
 pending[0].resolve({json:async()=>({entries:['wrong repository']})});await old;
 assert.deepEqual(renders.map(r=>r.path),['/repo/.worktrees/task']);
 const offline=ctx.loadFiles('/old');pending[2].reject(new Error('offline'));await new Promise(resolve=>setImmediate(resolve));
 const newer=ctx.loadFiles('/new');pending[3].resolve({json:async()=>({entries:['new']})});await newer;
 cache[0]({type:'dir',data:{entries:['stale cached']},ts:1});await offline;
 assert.deepEqual(renders.map(r=>r.path),['/repo/.worktrees/task','/new']);
});


test('task review artifacts retain task identity rather than implying project acceptance',()=>{
 const ctx=vm.createContext({});
 vm.runInContext(source.slice(source.indexOf('function _projectAssets('),source.indexOf('function _projectTaskDisplay(')),ctx);
 const assets=ctx._projectAssets({cards:[{id:'TASK-1',title:'Verify collection views'}],acceptance:{review_assets:[{task:'TASK-1',asset:{path:'candidate.md'}},{asset:{path:'acceptance.md'}}]}});
 assert.equal(assets[0].card.title,'Verify collection views');
 assert.equal(assets[1].card.title,'Project acceptance');
 assert.equal(assets[0].acceptance,true);
});


test('a checked task candidate cannot imply its integrated runtime goal is verified',()=>{
 const ctx=vm.createContext({});
 vm.runInContext(source.slice(source.indexOf('function _projectTaskDisplay('),source.indexOf('function _projectOutcomeVerdict(')),ctx);
 const card={phase:'verified',acceptance_criteria:['contract:image'],execution_plan:{execution:{stage:'verified'}}};
 const acceptance={criteria:[{id:'image',verifier:{type:'execution'},result:null}]};
 assert.equal(ctx._projectTaskDisplay(card,acceptance).label,'Candidate ready');
 assert.match(ctx._projectTaskDisplay(card,acceptance).detail,/runtime verification pending/);
 acceptance.criteria[0].result={state:'failed'};
 assert.equal(ctx._projectTaskDisplay(card,acceptance).label,'Candidate ready');
 acceptance.criteria[0].result={state:'passed'};
 assert.equal(ctx._projectTaskDisplay(card,acceptance).label,'Verified');
 assert.equal(ctx._projectTaskDisplay({...card,phase:'working',execution_plan:{execution:{stage:'working'}}},acceptance).label,'Assigned to worker');
});


test('a reserved worker is preparing, not expired evidence or a stopped executor',()=>{
 const ctx=vm.createContext({sessions:[],_expiredWorkerInventory:new Map()});
 vm.runInContext(source.slice(source.indexOf('function _projectWorkerRuntime('),source.indexOf('async function _projectResumeWorker(')),ctx);
 const worker={name:'new-worker',lifecycle:'missing',tasks:[{stage:'reserved'}]};
 assert.equal(ctx._projectWorkerRuntime(worker).label,'Preparing worker');
 assert.equal(ctx._projectWorkerRuntime({...worker,tasks:[{stage:'verified'}]}).label,'Evidence only');
 ctx.sessions.push({name:'new-worker',lifecycle:'active',status:'stopped',running:false});
 assert.equal(ctx._projectWorkerRuntime(worker).label,'Preparing worker');
 ctx.sessions[0].lifecycle='paused';assert.equal(ctx._projectWorkerRuntime(worker).label,'Paused');
 ctx.sessions[0].lifecycle='active';ctx.sessions[0].running=true;ctx.sessions[0].status='active';
 assert.equal(ctx._projectWorkerRuntime(worker).label,'Working');
});


test('worker task rows preserve the same integrated-verification label as the board',()=>{
 const ctx=vm.createContext({});
 vm.runInContext(source.slice(source.indexOf('function _projectTaskDisplay('),source.indexOf('function _projectOutcomeVerdict(')),ctx);
 vm.runInContext(source.slice(source.indexOf('function _projectWorkers('),source.indexOf('function _projectWorkerRuntime(')),ctx);
 const data={workers:[{name:'runtime-worker',tasks:[{id:'T1',stage:'verified'}]}],cards:[{id:'T1',phase:'verified',acceptance_criteria:['contract:image'],execution_plan:{execution:{stage:'verified'}}}],acceptance:{criteria:[{id:'image',verifier:{type:'execution'},result:null}]}};
 assert.equal(ctx._projectWorkers(data)[0].tasks[0].display_label,'Candidate ready');
 data.acceptance.criteria[0].result={state:'passed'};
 assert.equal(ctx._projectWorkers(data)[0].tasks[0].display_label,'Verified');
 assert.equal(data.workers[0].tasks[0].display_label,undefined);
});


test('normal executor capacity queues do not make the project outcome failed',()=>{
 const ctx=vm.createContext({});
 vm.runInContext(source.slice(source.indexOf('function _projectOutcomeVerdict('),source.indexOf('function _projectOutcomeCard(')),ctx);
 const data={acceptance:{state:'pending',criteria:[{id:'runtime',verifier:{type:'execution'}}]},cards:[{phase:'working'},{phase:'waiting',execution_plan:{waiting_reason:'executor_capacity',waiting_label:'Waiting for executor capacity'}},{phase:'waiting',execution_plan:{waiting_reason:'required_output:T1'}}]};
 assert.equal(ctx._projectOutcomeVerdict(data).tone,'running');
 data.cards[1].execution_plan.waiting_reason='authorization_required';
 assert.equal(ctx._projectOutcomeVerdict(data).tone,'failed');
 data.acceptance.state='failed';data.cards[1].execution_plan.waiting_reason='executor_capacity';
 assert.equal(ctx._projectOutcomeVerdict(data).tone,'failed');
});


test('project checkout navigation follows active work before an alphabetically earlier idle owner',()=>{
 const ctx=vm.createContext({_projectWorkers:data=>data.workers});
 vm.runInContext(source.slice(source.indexOf('function _projectPrimaryWorkspace('),source.indexOf('function _projectInventoryState(')),ctx);
 const data={workers:[{name:'a-idle',workspace_available:true,workspace:{path:'/repo/.worktrees/idle'},tasks:[{phase:'waiting'}]},{name:'z-working',workspace_available:true,workspace:{path:'/repo/.worktrees/active'},tasks:[{phase:'working'}]}]};
 assert.equal(ctx._projectPrimaryWorkspace(data).path,'/repo/.worktrees/active');
 data.workers[1].workspace_available=false;
 assert.equal(ctx._projectPrimaryWorkspace(data).path,'/repo/.worktrees/idle');
});


test('project wait cards show the authorization cause without leaking scheduler tokens',()=>{
 const ctx=vm.createContext({});
 vm.runInContext(source.slice(source.indexOf('function _projectTaskDisplay('),source.indexOf('function _projectOutcomeVerdict(')),ctx);
 const display=(reason,waiting)=>ctx._projectTaskDisplay({phase:'waiting',execution_plan:{waiting_label:'Waiting',waiting_reason:reason,execution:{waiting}}});
 assert.equal(display('executor_capacity','old failure').detail,'');
 assert.equal(display('required_output:T1','old failure').detail,'');
 assert.equal(display('authorization_required','spend: Production backfill needs approval').detail,'spend: Production backfill needs approval');
 assert.equal(display('verification failed (test): assertion mismatch').detail,'verification failed (test): assertion mismatch');
});


test('assigned project tasks require live activity before claiming working now',()=>{
 const ctx=vm.createContext({sessions:[{name:'worker',status:'active',running:true}],_initialLoad:false,_sessionLoadError:null,online:true});
 vm.runInContext(source.slice(source.indexOf('function _projectTaskDisplay('),source.indexOf('function _projectOutcomeVerdict(')),ctx);
 const card={phase:'working',execution_plan:{execution:{stage:'working',worker:'worker'}}};
 const label=()=>ctx._projectTaskDisplay(card).label;
 assert.equal(label(),'Working now');
 for(const [status,expected] of Object.entries({waiting:'Worker needs input',idle:'Waiting for worker',starting:'Starting worker',stopped:'Worker stopped',error:'Worker error',rate_limited:'Worker rate limited'})){
  ctx.sessions[0].status=status;assert.equal(label(),expected);assert.notEqual(ctx._projectTaskDisplay(card).cls,'working');
 }
 ctx.sessions[0].status='active';ctx.sessions[0].paused=true;assert.equal(label(),'Worker paused');
 ctx.sessions[0].paused=false;ctx._sessionLoadError={status:503};assert.equal(label(),'Worker state unavailable');
 ctx._sessionLoadError=null;ctx._initialLoad=true;assert.equal(label(),'Worker state unavailable');
 ctx._initialLoad=false;ctx.online=false;assert.equal(label(),'Worker state unavailable');
 ctx.online=true;ctx.sessions=[];assert.equal(label(),'Assigned to worker');
 card.execution_plan.execution.stage='reserved';assert.equal(label(),'Queued to worker');assert.notEqual(ctx._projectTaskDisplay(card).cls,'working');
});


test('task inspector retains the complete authorization diagnostic',()=>{
 const reason='spend: Production backfill needs approval. '+ 'The local candidate is still preparable. '.repeat(12)+'Unapproved action: materialize the production bucket.';
 const card={id:'A',title:'Cutover',phase:'waiting',acceptance_criteria:[],execution_plan:{waiting_label:'Authorization required',waiting_reason:'authorization_required',execution:{stage:'waiting',waiting:reason}}};
 const data={project:{name:'sample',policy:{executor:{provider:'codex',model:'gpt-6-luna'}}},cards:[card],acceptance:{state:'pending'}};
 const box={dataset:{},scrollTop:0,contains:()=>false};
 const ctx=vm.createContext({document:{getElementById:()=>box,activeElement:null},_projectStorage:()=> 'A',_projectOpenState:new Map(),_projectClip:(s,n)=>String(s).slice(0,n),esc:String,escJs:String,_projectAssetLinks:()=>'',_projectsData:data});
 vm.runInContext(source.slice(source.indexOf('function _projectTaskDisplay('),source.indexOf('function _projectOutcomeVerdict(')),ctx);
 vm.runInContext(source.slice(source.indexOf('function _projectCriteria('),source.indexOf('function _projectAssetLinks(')),ctx);
 ctx._projectInspectorRender(data);
 assert.ok(box.innerHTML.includes('<pre class="project-diagnostic" tabindex="0">'+reason+'</pre>'));
 assert.ok(box.innerHTML.includes('Full diagnostics ('+reason.length.toLocaleString()+' characters)'));
 assert.ok(!box.innerHTML.includes('<pre class="project-diagnostic" tabindex="0">authorization_required</pre>'));
});
