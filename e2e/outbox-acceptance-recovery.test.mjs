import http from 'node:http';
import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source=fs.readFileSync(process.env.AMUX_OUTBOX_SOURCE || 'crates/amux-dashboard/static/app.js','utf8');
function section(start,end){const a=source.indexOf(start),b=source.indexOf(end,a);assert(a>=0 && b>a, start);return source.slice(a,b);}
const run=section('function _outboxMessageProgress(', 'async function _syncOneDraft(');
const helpers=source.includes('function _outboxMessageId(') ? section('function _outboxMessageId(', '// Queue modal') : '';
const reviewAction=section('const _outboxManualAction =', '\n');
// AMUX-4844. THE SANDBOX IS HAND-MAINTAINED, SO MAKE ITS GAPS SAY SO.
//
// This file does not load app.js. It slices it by text markers and evaluates
// the slice in a vm whose globals are the object literal in harness(). Every
// app.js internal the slice CALLS but does not DEFINE has to be provided there.
//
// When one is missing, v8 throws `ReferenceError: <name> is not defined` from
// `evalmachine.<anonymous>`. That message is actively misleading, because the
// name is usually defined perfectly well in app.js, just OUTSIDE the slice. It
// cost this repo eight consecutive red `rust` runs on main: `_syncBannerBeacon`
// is defined at app.js:1017 and called from inside the sliced region (:3003,
// :3135), so the sandbox never saw it. Nothing pointed at this file, and the
// red gate also silently stopped every cloud deploy, because
// deploy-cloud.yml only runs when `rust` concludes success.
//
// NINE MORE NAMES ARE MISSING THE SAME WAY right now (_syncOneDraft,
// _validateBoardAcknowledgement, _peekMessagesRender and friends). They are
// invisible only because no test reaches those branches, so each one is the
// next incident waiting for someone to touch the right line.
//
// NOT FIXED BY STUBBING THEM ALL AS NO-OPS. A silent no-op on a branch a future
// test does depend on would pass while doing nothing, which is worse than the
// red. So every missing name gets a stub that THROWS, naming itself and this
// file. The branch still fails if it is reached; it just stops lying about why.
// A name that needs real behaviour gets a real stub in the context literal,
// which takes precedence over this because it is already `in ctx`.
//
// Scoped to the `_` prefix app.js uses for its internals, so a genuine global
// like Date or JSON is never matched.
function installMissingStubs(src,ctx){
 const defined=new Set([...src.matchAll(/(?:function|const|let|var)\s+(_[A-Za-z0-9_$]*)/g)].map(m=>m[1]));
 const called=new Set([...src.matchAll(/\b(_[A-Za-z0-9_$]*)\s*\(/g)].map(m=>m[1]));
 const missing=[...called].filter(n=>!defined.has(n)&&!(n in ctx)).sort();
 for(const name of missing){
  ctx[name]=(...args)=>{
   throw new Error(
    `${name}() was called by the sliced app.js, but e2e/outbox-acceptance-recovery.test.mjs `+
    `does not provide it. It is defined in app.js OUTSIDE the evaluated slice, so the vm `+
    `cannot see it. Add a stub for ${name} to the vm context in THIS file (${args.length} `+
    `arg(s) were passed); do not go looking for a missing definition in app.js.`);
  };
 }
 return missing;
}
function harness(queue,replies,transport=null){
 const requests=[],patches=[],signals=[],timers=[],beacons=[],peekNudges=[];
 const element={classList:{add(){},remove(){},contains(){return false;}},textContent:'',innerHTML:''};
 const ctx=vm.createContext({peekSession:'test-worker',_peekKickFast:()=>peekNudges.push(Date.now()),Date,Set,console,Response,JSON,navigator:{onLine:true},_upqList:async()=>[],_uploadSyncPending:false,_syncChecklist:[],_syncBannerRequested:false,_syncBannerAuto:false,_syncPillText:'',_syncChecklistAt:0,updateConnectionStatus(){},_clearSyncTransientToast(){},_syncBannerBeacon:(phase,items)=>beacons.push({phase,n:(items||[]).length}),encodeURIComponent,decodeURIComponent,document:{getElementById:()=>element},drafts:[],offlineQueue:queue,_outboxActive:new Set(),describeOp:()=> 'test send',esc:s=>s,
  _outboxLock:async(_,f)=>f(),_readQueue:()=>queue,_interactionReplay:()=>({id:'int-test'}),_outboxQueueable:()=>true,_mutateQueue:async f=>f(queue),_authHeaders:h=>h,
  _boundedMutationFetch:async(url,opts)=>{requests.push({url,opts});if(transport)return transport(url,opts);const r=replies.shift();assert(r,'unexpected request');if(r instanceof Error)throw r;return new Response(JSON.stringify(r.body),{status:r.status});},
  _apiErrText:async r=>(await r.json()).error,_interactionSet:(_,v)=>patches.push(v),_interactionAcknowledge:async()=>{},
  _outboxDiagnostic:(kind,data)=>signals.push({kind,...data}),amuxTrack(){},updateConnectionStatus(){},fetchSessions(){},fetchBoard(){},showToast(){},setTimeout:(f,ms)=>timers.push(ms),clearTimeout(){},_writeError:'',_syncRetryTimer:null,_syncBackoffMs:0,_SYNC_MIN_MS:2000,_SYNC_MAX_MS:60000});
 const evaluated=reviewAction+'\n'+section('function _validateMessageAcknowledgement(', 'function _localMessageRequest(')+helpers+run+section('function _scheduleSyncRetry()', 'function runSyncBanner(');
 installMissingStubs(evaluated,ctx);
 vm.runInContext(evaluated,ctx);
 return {queue,requests,patches,signals,timers,beacons,peekNudges,banner:element,drain:()=>vm.runInContext('_runSyncBanner(true)',ctx),schedule:()=>vm.runInContext('_scheduleSyncRetry()',ctx)};
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
 assert.equal(restored.requests[0].url,'/api/sessions/test-worker/send?msg_id=same-identity&text=continue');
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
test('a released reservation is sent once with the same identity (AMUX-4594)',async()=>{
 const h=harness([pending({delivery_uncertain:true})],[{status:200,body:{accepted:false,released:true,delivered:false,msg_id:'same-identity'}},{status:200,body:{ok:true,deduped:true,id:'sent-1'}}]);
 await h.drain();
 assert.equal(h.queue.length,0);
 assert.deepEqual(h.requests.map(r=>r.opts.method),['GET','POST']);
 assert.equal(JSON.parse(h.requests[1].opts.body).msg_id,'same-identity');
 assert(h.signals.some(s=>s.kind==='acceptance_released'&&s.measured===true));
});
test('an unavailable transcript remains recoverable without a second paste',async()=>{
 const h=harness([pending({state:'blocked',error:'409: previous message acceptance is uncertain'})],[{status:200,body:{accepted:false,stranded:true,delivered:'unknown',msg_id:'same-identity'}},accepted]);
 await h.drain();
 assert.equal(h.queue.length,1);assert.equal(h.queue[0].state,'pending');
 await h.drain();assert.equal(h.queue.length,0);assert(h.requests.every(r=>r.opts.method==='GET'));
 assert(h.signals.some(s=>s.kind==='acceptance_unknown'));
});
test('an uncertain send is still re-checked when a quiet sync runs beside it (d69efdef, AMUX-4594)',async()=>{
 const h=harness([pending({delivery_uncertain:true})],[waiting]);
 await h.drain();
 assert.equal(h.requests.length,1,'the stuck send was read again');
 assert.equal(h.requests[0].opts.method,'GET');
 assert.equal(h.queue.length,1);assert.equal(h.patches.at(-1).phase,'unknown');
});
test('a long outage never retires automatic receipt recovery',async()=>{
 const h=harness([pending({delivery_uncertain:true,checking_since:Date.now()-11*60000})],[waiting,accepted]);
 await h.drain();
 assert.equal(h.requests.length,1,'the verdict is read before giving up');
 assert.equal(h.queue.length,1);assert.equal(h.queue[0].state,'pending');
 await h.drain();assert.equal(h.queue.length,0);assert(h.requests.every(r=>r.opts.method==='GET'));
});
// AMUX-4910. A 5xx is retried because a server that FAILED may succeed next
// time. 501 is not that: it is the server saying the capability does not exist
// here, so the same request gets the same answer forever.
//
// THE SPECIMEN, measured 2026-09-20: one tick of the "Use worktree" checkbox,
// whose POST /api/sessions the server answers 501 with an exact remedy, was
// replayed 845 times at a dead-flat 4/min for over three hours. It generated
// enough 5xx on its own to trip the route.mounted_routes_answer invariant,
// where it was then filed as a SERVER fault (AMUX-4900) alongside four healthy
// routes. One client retry rule, three cards deep.
const worktreeCreate=()=>({id:'c1',url:'/api/sessions',options:{method:'POST',headers:{},body:JSON.stringify({name:'lc1-solo-haiku',worktree:true})},timestamp:Date.now()});
const notImplemented={status:501,body:{error:"worktree creation is not implemented on this server yet - uncheck 'Use worktree' to create a normal worker"}};
test('a 501 is terminal: one attempt, and the outbox never asks again (AMUX-4910)',async()=>{
 const h=harness([worktreeCreate()],[notImplemented]);
 await h.drain();
 assert.equal(h.requests.length,1,'exactly one attempt for a capability that does not exist');
 assert.equal(h.queue.length,1,'the operation is kept for the person to see, not silently dropped');
 assert.equal(h.queue[0].state,'blocked','501 must not stay retryable');
 assert.match(h.queue[0].error,/not implemented/,"the server's own remedy survives to the panel");
 // The real defect was the SECOND attempt, and the 845th. A fresh replay over
 // the stored queue must issue no request at all: harness() asserts on any
 // request it has no reply for, so an extra attempt fails loudly here.
 const again=harness(JSON.parse(JSON.stringify(h.queue)),[]);
 await again.drain();
 assert.equal(again.requests.length,0,'a blocked 501 is never replayed');
});
test('a transient 5xx is still retried, so the 501 rule did not blunt recovery (AMUX-4910)',async()=>{
 const h=harness([worktreeCreate()],[{status:503,body:{error:'service unavailable'}}]);
 await h.drain();
 assert.equal(h.requests.length,1);
 assert.notEqual(h.queue[0].state,'blocked','503 means this attempt failed, not that the capability is absent');
 const again=harness(JSON.parse(JSON.stringify(h.queue)),[{status:201,body:{ok:true,name:'lc1-solo-haiku'}}]);
 await again.drain();
 assert.equal(again.requests.length,1,'a transient failure is retried');
 assert.equal(again.queue.length,0,'and clears when it succeeds');
});

test('a concurrent pending send moves to receipt polling instead of repeated POSTs',async()=>{
 const h=harness([pending()],[{status:503,body:{submission:'pending',error:'still pending'}},accepted]);
 await h.drain();await h.drain();
 assert.equal(h.queue.length,0);assert.deepEqual(h.requests.map(r=>r.opts.method),['POST','GET']);
});
test('previously timed-out messages recover after reload without manual retry',async()=>{
 const h=harness([pending({state:'blocked',error:'Confirmation timed out after 11m. Dismiss or retry.'})],[accepted]);
 await h.drain();assert.equal(h.queue.length,0);assert.equal(h.requests[0].opts.method,'GET');
});

// Real TCP faults against the shipped replay loop. The fixture server models
// only the durable receipt protocol; Rust tests cover the actual reservation
// implementation separately. No production server or worker is interrupted.
test('TCP outage, reload, lost ACK, restart and flapping preserve identity and FIFO',async()=>{
 const acceptedIds=new Map(), deliveries=[];
 let server,port,loseAck=false,unavailable=false;
 const start=async()=>{
  server=http.createServer(async(req,res)=>{
   if(unavailable){res.writeHead(503);res.end(JSON.stringify({error:'restarting'}));return;}
   const url=new URL(req.url,'http://fixture');
   let body='';for await(const chunk of req)body+=chunk;
   if(req.method==='GET'){
    const id=url.searchParams.get('msg_id'),receipt=acceptedIds.get(id);
    res.end(JSON.stringify(receipt?{accepted:true,msg_id:id,id:receipt}:{accepted:false,released:true,msg_id:id}));return;
   }
   const id=JSON.parse(body).msg_id;
   if(!acceptedIds.has(id)){acceptedIds.set(id,'receipt-'+id);deliveries.push(id);}
   if(loseAck){loseAck=false;req.socket.destroy();return;}
   res.end(JSON.stringify({ok:true,deduped:true,id:acceptedIds.get(id)}));
  });
  await new Promise(resolve=>server.listen(port||0,'127.0.0.1',resolve));port=server.address().port;
 };
 const stop=async()=>{server.closeAllConnections();await new Promise(resolve=>server.close(resolve));};
 const transport=(url,opts)=>fetch('http://127.0.0.1:'+port+url,{...opts,signal:AbortSignal.timeout(500)});
 await start();await stop();
 const one=pending(),two=pending({id:'q2',options:{method:'POST',body:JSON.stringify({text:'second',msg_id:'second-id'})}});
 let h=harness([one,two],[],transport);
 try{
  await h.drain();assert.equal(h.queue.length,2);assert.equal(deliveries.length,0);
  // Browser reload reconstructs only durable data, not in-memory state.
  h=harness(JSON.parse(JSON.stringify(h.queue)),[],transport);
  await start();loseAck=true;
  await h.drain();assert.equal(h.queue.length,2);assert.deepEqual(deliveries,['same-identity']);
  // The server restarts after accepting but before the client got its ACK.
  await stop();await h.drain();await start();unavailable=true;
  await h.drain();assert.equal(h.queue.length,2);unavailable=false;
  await h.drain();assert.equal(h.queue.length,0);
  assert.deepEqual(deliveries,['same-identity','second-id'],'exactly once, ordered across all faults');
  assert(h.requests.filter(r=>r.opts.method==='POST').every(r=>['same-identity','second-id'].includes(JSON.parse(r.opts.body).msg_id)));
 }finally{if(server.listening)await stop();}
});

export {harness, pending};

test('worker startup races retry automatically, but authorization refusals do not',async()=>{
 const h=harness([pending()],[{status:409,body:{retryable:true,submitted:false,error:'worker is still starting'}},{status:200,body:{ok:true,submitted:true,id:'receipt-start'}}]);
 await h.drain();assert.notEqual(h.queue[0].state,'blocked');
 await h.drain();assert.equal(h.queue.length,0);
 const denied=harness([pending()],[{status:403,body:{error:'not authorized'}}]);
 await denied.drain();assert.equal(denied.queue[0].state,'blocked');await denied.drain();assert.equal(denied.requests.length,1);
});

test('retained project draft and approval operations never replay automatically',async()=>{
 const h=harness([pending({url:'/api/projects/draft'}),pending({id:'approve',url:'/api/projects/sample/acceptance/approve'})],[]);
 await h.drain();assert.equal(h.requests.length,0);assert.equal(h.queue.length,2);
});


test('reconnect replay wakes the visible terminal and measures acknowledged delivery without prompt content',async()=>{
 const q=pending({timestamp:Date.now()-60000,attempts:2});
 const h=harness([q],[{status:200,body:{ok:true,submitted:true}}]);
 await h.drain();
 assert.equal(h.queue.length,0);
 assert.equal(h.peekNudges.length,2,'wake on dispatch and acknowledgement, after old input burst expired');
 const delivered=h.signals.find(s=>s.kind==='message_delivery_acknowledged');
 assert.equal(delivered.msg_id,'same-identity');assert.equal(delivered.worker,'test-worker');
 assert(delivered.queued_ms>=60000);assert(delivered.attempt_ms>=0);assert.equal(delivered.attempts,3);
 assert(!JSON.stringify(h.signals).includes('continue'),'prompt text is not telemetry');
});

test('an offline failure retains every send in order and only acknowledges them after recovery',async()=>{
 const q1=pending();const q2=pending({id:'q2',options:{method:'POST',body:JSON.stringify({text:'second',msg_id:'second-id'})}});
 const h=harness([q1,q2],[new Error('offline')]);await h.drain();
 assert.equal(h.queue.length,2);assert.equal(h.requests.length,1,'same worker stays ordered after failure');
 assert(!h.signals.some(s=>s.kind==='message_delivery_acknowledged'));
 const restored=harness(JSON.parse(JSON.stringify(h.queue)),[{status:200,body:{ok:true,submitted:true}},{status:200,body:{ok:true,submitted:true}}]);
 await restored.drain();assert.equal(restored.queue.length,0);
 assert.deepEqual(restored.requests.map(r=>JSON.parse(r.opts.body).msg_id),['same-identity','second-id']);
 assert.equal(restored.signals.filter(s=>s.kind==='message_delivery_acknowledged').length,2);
});
