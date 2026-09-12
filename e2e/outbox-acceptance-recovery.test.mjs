import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source=fs.readFileSync(process.env.AMUX_OUTBOX_SOURCE || 'crates/amux-dashboard/static/app.js','utf8');
function section(start,end){const a=source.indexOf(start),b=source.indexOf(end,a);assert(a>=0 && b>a, start);return source.slice(a,b);}
const run=section('async function _runSyncBanner(', 'async function _syncOneDraft(');
const helpers=source.includes('function _outboxMessageId(') ? section('function _outboxMessageId(', '// Queue modal') : '';
function harness(queue,replies){
 const requests=[],patches=[],signals=[],timers=[];
 const element={classList:{add(){},remove(){},contains(){return false;}},textContent:'',innerHTML:''};
 const ctx=vm.createContext({Date,Set,console,Response,JSON,navigator:{onLine:true},_upqList:async()=>[],_uploadSyncPending:false,_syncChecklist:[],_clearSyncTransientToast(){},encodeURIComponent,decodeURIComponent,document:{getElementById:()=>element},drafts:[],offlineQueue:queue,_outboxActive:new Set(),describeOp:()=> 'test send',esc:s=>s,
  _outboxLock:async(_,f)=>f(),_readQueue:()=>queue,_interactionReplay:()=>({id:'int-test'}),_outboxQueueable:()=>true,_mutateQueue:async f=>f(queue),_authHeaders:h=>h,
  _boundedMutationFetch:async(url,opts)=>{requests.push({url,opts});const r=replies.shift();assert(r,'unexpected request');if(r instanceof Error)throw r;return new Response(JSON.stringify(r.body),{status:r.status});},
  _apiErrText:async r=>(await r.json()).error,_interactionSet:(_,v)=>patches.push(v),_interactionAcknowledge:async()=>{},_validateMessageAcknowledgement:r=>assert.equal(r.deduped,true),
  _outboxDiagnostic:(kind,data)=>signals.push({kind,...data}),amuxTrack(){},updateConnectionStatus(){},fetchSessions(){},fetchBoard(){},showToast(){},setTimeout:(f,ms)=>timers.push(ms),clearTimeout(){},_writeError:'',_syncRetryTimer:null,_syncBackoffMs:0,_SYNC_MIN_MS:2000,_SYNC_MAX_MS:60000});
 vm.runInContext(helpers+run+section('function _scheduleSyncRetry()', 'function runSyncBanner('),ctx);
 return {queue,requests,patches,signals,timers,banner:element,drain:()=>vm.runInContext('_runSyncBanner(true)',ctx),schedule:()=>vm.runInContext('_scheduleSyncRetry()',ctx)};
}
function pending(extra={}) {return {id:'q1',url:'/api/sessions/test-worker/send',options:{method:'POST',headers:{},body:JSON.stringify({text:'continue',msg_id:'same-identity'})},timestamp:Date.now(),...extra};}
const waiting={status:202,body:{accepted:false,msg_id:'same-identity'}};
const accepted={status:200,body:{accepted:true,msg_id:'same-identity',id:'receipt-1'}};
test('legacy uncertain send resumes automatic confirmation after reload, without any POST',async()=>{
 const q=pending({state:'blocked',error:'409: previous message acceptance is uncertain',timestamp:Date.now()-9*86400000});
 const h=harness([q],[waiting]);h.schedule();assert.deepEqual(h.timers,[2000]);await h.drain();
 assert.equal(h.queue.length,1);assert.equal(h.patches.at(-1).phase,'unknown');assert.equal(h.patches.at(-1).measured,false);
 const restored=harness(JSON.parse(JSON.stringify(h.queue)),[accepted]);await restored.drain();
 assert.equal(restored.queue.length,0);assert(h.requests.concat(restored.requests).every(r=>r.opts.method==='GET'));
 assert.equal(restored.requests[0].url,'/api/sessions/test-worker/send?msg_id=same-identity');
 assert(restored.signals.some(s=>s.kind==='acceptance_recovered'&&s.measured===true));
});
test('fresh uncertain response retries reads, preserving identity and later queued sends',async()=>{
 const h=harness([pending(),pending({id:'q2'})],[{status:409,body:{submission:'uncertain',error:'acceptance is uncertain'}},waiting]);
 await h.drain();assert.match(h.banner.textContent,/1 awaiting confirmation, 1 waiting/);assert.doesNotMatch(h.banner.textContent,/failed/);assert.equal(h.queue.length,2);assert.equal(h.requests.length,1);assert.equal(h.patches.at(-1).phase,'unknown');
 await h.drain();assert.equal(h.requests.length,2);assert.equal(h.requests[1].opts.method,'GET');assert.equal(h.queue.length,2);
 assert.equal(JSON.parse(h.queue[0].options.body).msg_id,'same-identity');
});
test('wrong acknowledgement and read failure keep uncertainty durable and retries bounded',async()=>{
 const h=harness([pending({delivery_uncertain:true})],[{status:200,body:{accepted:true,msg_id:'other',id:'wrong'}},new Error('network down')]);
 await h.drain();await h.drain();assert.equal(h.queue.length,1);assert.equal(h.patches.at(-1).phase,'unknown');
 for(let i=0;i<10;i++)h.schedule();assert.equal(h.timers.at(-1),60000);
 assert(h.requests.every(r=>r.opts.method==='GET'));assert(!h.signals.some(s=>s.kind==='acceptance_recovered'));
});
test('unkeyed uncertain message and unrelated refused edits stay blocked',async()=>{
 const h=harness([pending({state:'blocked',error:'delivery unconfirmed',options:{method:'POST',body:'{"text":"continue"}'}}),pending({id:'board',url:'/api/board/AF-1',state:'blocked',error:'acceptance is uncertain'})],[]);
 await h.drain();h.schedule();assert.equal(h.queue.length,2);assert.equal(h.requests.length,0);assert.equal(h.timers.length,0);
});
test('steering confirmations use the server transport namespace',async()=>{
 const h=harness([pending({url:'/api/sessions/test-worker/steer',delivery_uncertain:true})],[{status:200,body:{accepted:true,msg_id:'steer:same-identity',id:'steering-row'}}]);
 await h.drain();assert.equal(h.queue.length,0);assert.equal(h.requests[0].url,'/api/sessions/test-worker/send?msg_id=steer%3Asame-identity');assert.equal(h.requests[0].opts.method,'GET');
});
