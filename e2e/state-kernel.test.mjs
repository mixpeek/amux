import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createInteractions, createInteractionPoller, classify, commandFor, phases } from '../crates/amux-dashboard/static/state/interactions.mjs';
import { createStore } from '../crates/amux-dashboard/static/state/store.mjs';
import { createQueries } from '../crates/amux-dashboard/static/state/query.mjs';
import { createSync, invalidationKeys } from '../crates/amux-dashboard/static/state/sync.mjs';
import { uploadActor } from '../crates/amux-dashboard/static/state/upload.mjs';
import { projectEvent } from '../crates/amux-dashboard/static/state/agui.mjs';
import { actionFromHandler } from '../crates/amux-dashboard/static/state/controls.mjs';
import { createEffectReconciler } from '../crates/amux-dashboard/static/state/effects.mjs';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { parse } from 'espree';

const command = () => commandFor('PATCH','/api/board/AR-142');
const memory = () => { const data = new Map(); return {getItem:k => data.get(k), setItem:(k,v) => data.set(k,v)}; };
test('acknowledgements distinguish queued, refusal, no-op and long-running acceptance', () => {
  assert.equal(classify(202,{},true).phase,'queued');
  assert.equal(classify(202,{}).phase,'running');
  assert.equal(classify(200,{queued:true}).phase,'queued');
  assert.equal(classify(200,{ignored_fields:['status']}).phase,'refused');
  assert.equal(classify(200,{applied:false}).phase,'noop');
  assert.equal(classify(409,{error:'Evidence required'}).message,'Evidence required');
  assert.equal(classify(500,{}).phase,'failed');
  assert.equal(classify(200,{}).phase,'unknown');
  assert.equal(classify(204,{}).phase,'unknown');
  for (const phase of phases) assert.equal(classify(200,{phase}).phase,phase);
  assert.equal(classify(409,{phase:'applied'}).phase,'refused');
});
test('receipt stays sending until JSON body has been interpreted; effects require authority', async () => {
  const ledger = createInteractions();
  const receipt = ledger.accept({command:command()});
  ledger.update(receipt.id,{phase:'sending'});
  let finish;
  const response = {status:200, headers:new Headers({'content-type':'application/json'}), clone:() => ({json:() => new Promise(r => {finish=r;})})};
  const acknowledgement = ledger.acknowledge(receipt.id,response);
  assert.equal(ledger.get(receipt.id).phase,'sending');
  finish({applied:false, rev:42});
  await acknowledgement;
  assert.equal(ledger.get(receipt.id).phase,'noop');
  assert.equal(ledger.get(receipt.id).effects.length,0);
});
test('all requested reducer paths preserve one identity and survive reload', () => {
  const paths = [ ['sending','applied'], ['queued','sending','applied'], ['queued','running','waiting','running','applied'],
    ['running','blocked'], ['sending','refused'], ['queued','failed'], ['noop'] ];
  for (const path of paths) {
    const storage = memory(); const ledger = createInteractions({storage});
    const receipt = ledger.accept({command:command()});
    for (const phase of path) ledger.update(receipt.id,{phase});
    assert.equal(createInteractions({storage}).get(receipt.id).phase,path.at(-1));
  }
});
test('reload and timeout expose uncertainty; terminal receipts never expire', () => {
  let now=1; const signals=[]; const storage=memory();
  const ledger=createInteractions({storage,now:()=>now,diagnostic:e=>signals.push(e)});
  const receipt=ledger.accept({command:command()});
  ledger.update(receipt.id,{phase:'sending'});
  assert.equal(createInteractions({storage}).get(receipt.id).phase,'unknown');
  now=200000; ledger.expire();
  assert.equal(ledger.get(receipt.id).measured,false);
  assert.equal(signals.at(-1).verdict,'unknown');
  ledger.update(receipt.id,{phase:'reconciled'});
  now+=200000; ledger.expire();
  assert.equal(ledger.get(receipt.id).phase,'reconciled');
});
test('multiple effects retain cause and dedupe without changing receipt phase', () => {
  const ledger=createInteractions(); const receipt=ledger.accept({command:command()});
  ledger.effect(receipt.id,{id:'a',kind:'task.updated'});
  ledger.effect(receipt.id,{id:'b',kind:'worker.resumed'});
  ledger.effect(receipt.id,{id:'b',kind:'worker.resumed'});
  const result=ledger.get(receipt.id);
  assert.equal(result.effects.length,2);
  assert.ok(result.effects.every(e=>e.interaction_id===receipt.id));
  assert.equal(result.phase,'accepted');
  result.effects.length=0;
  assert.equal(ledger.get(receipt.id).effects.length,2);
});
test('bounded history never evicts unresolved commands', () => {
  const ledger=createInteractions({max:2});
  const pending=ledger.accept({command:command()});
  for(let n=0;n<5;n++) { const r=ledger.accept({command:command()}); ledger.update(r.id,{phase:'applied'}); }
  assert.equal(ledger.get(pending.id).phase,'accepted');
});
test('reload also preserves unresolved receipts beyond the settled-history limit',()=>{
  const storage=memory(); const ledger=createInteractions({storage,max:2});
  const pending=[];
  for(let n=0;n<4;n++) pending.push(ledger.accept({command:command()}));
  const restored=createInteractions({storage,max:2});
  assert.ok(pending.every(r=>restored.get(r.id)?.phase==='unknown'));
});
test('query cache deduplicates concurrent reads and targets invalidation', async () => {
  const query=createQueries(); let calls=0;
  const fetcher=async()=>{calls++; return new Response('{"ok":true}');};
  const [a,b]=await Promise.all([query.response(['board','response'],fetcher),query.response(['board','response'],fetcher)]);
  assert.equal(calls,1); assert.notEqual(a,b);
  assert.equal(await a.text(),await b.text());
  query.set(['sessions'],[]); query.set(['board'],[]);
  await query.invalidate(['board']);
  assert.equal(query.state(['board']).isInvalidated,true);
  assert.equal(query.state(['sessions']).isInvalidated,false);
  query.client.clear();
});
test('status polling rotates beyond both the batch and history limits', async () => {
  const ledger = createInteractions();
  const ids = Array.from({length:205}, () => {
    const receipt = ledger.accept({command:command(), request:{method:'PATCH'}});
    ledger.update(receipt.id,{phase:'unknown'});
    return receipt.id;
  });
  const reads = [];
  const poll = createInteractionPoller({interactions:ledger,
    read:async id => { reads.push(id); return {phase:'unknown',measured:false}; },
    reconcile:async()=>{}, diagnostic:assert.fail});
  for (let i=0;i<26;i++) await poll();
  assert.deepEqual(reads.slice(0,205),ids);
  assert.equal(new Set(reads).size,205);
});
test('status polling includes server queues but leaves the device outbox to replay', async () => {
  const ledger=createInteractions();
  const local=ledger.accept({command:command(),request:{method:'PATCH'}});
  const server=ledger.accept({command:command(),request:{method:'PATCH'}});
  for (const receipt of [local,server]) await ledger.acknowledge(receipt.id,new Response('{"queued":true}',
    {status:202,headers:{'content-type':'application/json',...(receipt===local?{'X-Amux-Outbox':'queued'}:{})}}));
  const reads=[];
  const poll=createInteractionPoller({interactions:ledger,
    read:async id=>{reads.push(id);return {phase:'applied',measured:true};},
    reconcile:async()=>{},diagnostic:assert.fail});
  await poll();
  assert.deepEqual(reads,[server.id]);
  assert.equal(ledger.get(local.id).phase,'queued');
  assert.equal(ledger.get(server.id).phase,'applied');
});
test('failed status probes announce themselves without blocking peers or same-phase effects', async () => {
  const ledger=createInteractions();
  const bad=ledger.accept({command:command()});
  const good=ledger.accept({command:command()});
  ledger.update(good.id,{phase:'running'});
  const signals=[];const effects=[];
  const poll=createInteractionPoller({interactions:ledger,
    read:async id=>{if(id===bad.id)throw new Error('HTTP 404');return {phase:'running',measured:true};},
    reconcile:async id=>effects.push(id),diagnostic:e=>signals.push(e)});
  await poll();
  assert.deepEqual(effects,[good.id]);
  assert.equal(signals[0].verdict,'interaction_status_poll_failed');
  assert.equal(signals[0].interaction_id,bad.id);
  assert.equal(signals[0].n_considered,1);
  assert.equal(signals[0].pending_count,2);
});
test('status polling cannot overwrite a newer local acknowledgement', async () => {
  const ledger=createInteractions({now:()=>1});
  const receipt=ledger.accept({command:command()});
  const poll=createInteractionPoller({interactions:ledger,read:async()=>{
    ledger.update(receipt.id,{phase:'applied'});
    return {phase:'running',measured:true};
  },reconcile:assert.fail,diagnostic:assert.fail});
  await poll();
  assert.equal(ledger.get(receipt.id).phase,'applied');
});
test('unchanged status probes reconcile effects without repeating outcome diagnostics', async () => {
  const signals=[];const ledger=createInteractions({diagnostic:e=>signals.push(e)});
  const receipt=ledger.accept({command:command()});
  ledger.update(receipt.id,{phase:'unknown',measured:false});
  signals.length=0;
  let reconciled=0;
  const poll=createInteractionPoller({interactions:ledger,
    read:async()=>({phase:'unknown',measured:false}),
    reconcile:async()=>{reconciled++;},diagnostic:assert.fail});
  await poll();await poll();
  assert.equal(reconciled,2);
  assert.deepEqual(signals,[]);
});
test('review: a successful replay clears reload and timeout uncertainty', async () => {
  const storage=memory();let time=1;const signals=[];
  const diagnostic=event=>signals.push(event);
  let ledger=createInteractions({storage,now:()=>time,diagnostic});
  const receipt=ledger.accept({command:command()});
  ledger.update(receipt.id,{phase:'sending'});
  ledger=createInteractions({storage,now:()=>time,diagnostic});
  for (const expired of [false,true]) {
    ledger.update(receipt.id,{phase:'sending'});
    if(expired){time+=200000;ledger.expire();}
    await ledger.acknowledge(receipt.id,new Response('{"applied":true}',{headers:{'content-type':'application/json'}}));
    assert.equal(ledger.get(receipt.id).phase,'applied');
    assert.equal(ledger.get(receipt.id).measured,true);
    assert.equal(ledger.get(receipt.id).why_unmeasured,undefined);
  }
  assert.equal(signals.filter(e=>e.verdict==='interaction_measurement_recovered').length,2);
});
test('review: settled receipts still recover effect reads without executing commands', async () => {
  const ledger=createInteractions();const receipt=ledger.accept({command:command(),request:{method:'PATCH'}});
  ledger.update(receipt.id,{phase:'applied'});
  let reconciled=0;
  const poll=createInteractionPoller({interactions:ledger,read:assert.fail,
    reconcile:async()=>{reconciled++;},diagnostic:assert.fail});
  await poll();
  assert.equal(reconciled,1);
  assert.equal(ledger.get(receipt.id).phase,'applied');
});
test('review: a hanging effects read cannot hold the poller flight flag forever', async () => {
  const ledger=createInteractions();
  const receipts=[ledger.accept({command:command()}),ledger.accept({command:command()})];
  const reads=[];const signals=[];
  const poll=createInteractionPoller({interactions:ledger,timeoutMs:10,
    read:async id=>{reads.push(id);return {phase:'running',measured:true};},
    reconcile:async id=>{if(id===receipts[0].id)await new Promise(()=>{});},diagnostic:e=>signals.push(e)});
  const completed=await Promise.race([poll().then(()=>true),new Promise(resolve=>setTimeout(()=>resolve(false),100))]);
  assert.equal(completed,true);
  assert.deepEqual(reads,receipts.map(r=>r.id));
  assert.equal(signals[0].verdict,'interaction_status_poll_failed');
  await poll();
  assert.equal(reads.length,4);
});
test('effects deadline covers an unfinished response body, aborts it, and records failure', async () => {
  const ledger=createInteractions();const receipt=ledger.accept({command:command()});
  ledger.update(receipt.id,{phase:'applied'});
  const signals=[];let signal;let finish;
  const reconcile=createEffectReconciler({interactions:ledger,timeoutMs:10,diagnostic:e=>signals.push(e),
    read:async(_,s)=>{signal=s;return {json:()=>new Promise(resolve=>{finish=resolve;})}.json();}});
  await assert.rejects(reconcile(receipt.id),/timed out/);
  assert.equal(signal.aborted,true);
  assert.equal(ledger.get(receipt.id).phase,'applied');
  assert.equal(ledger.get(receipt.id).effect_sync.phase,'failed');
  assert.equal(ledger.get(receipt.id).effect_sync.measured,false);
  assert.equal(signals[0].verdict,'interaction_reconcile_failed');
  finish({measured:true,n_considered:1,effects:[{id:'late'}]});
  await new Promise(resolve=>setTimeout(resolve,0));
  assert.deepEqual(ledger.get(receipt.id).effects,[]);
});
test('failed effects recover after reload and discover later detached writes within the retained history', async () => {
  const storage=memory();let time=1;let ledger=createInteractions({storage,now:()=>time});
  const receipt=ledger.accept({command:command(),request:{method:'PATCH'}});
  ledger.update(receipt.id,{phase:'applied'});
  let calls=0;const signals=[];
  const read=async()=>{
    calls++;
    if(calls===1)throw new Error('HTTP 503');
    const effects=calls===2?[{id:'first'}]:[{id:'first'},{id:'detached'}];
    return {measured:true,n_considered:effects.length,effects};
  };
  const create=()=>createEffectReconciler({interactions:ledger,read,diagnostic:e=>signals.push(e),now:()=>time,retryMs:10,refreshMs:100});
  let reconcile=create();
  await assert.rejects(reconcile(receipt.id),/503/);
  await reconcile(receipt.id);assert.equal(calls,1);
  ledger=createInteractions({storage,now:()=>time});reconcile=create();time+=10;
  const poll=createInteractionPoller({interactions:ledger,read:assert.fail,reconcile,diagnostic:assert.fail,now:()=>time});
  await poll();
  assert.equal(ledger.get(receipt.id).phase,'applied');
  assert.equal(ledger.get(receipt.id).effects.length,1);
  assert.equal(ledger.get(receipt.id).effect_sync.measured,true);
  assert.equal(signals.at(-1).verdict,'interaction_effects_recovered');
  await poll();assert.equal(calls,2);
  time+=100;await poll();
  assert.equal(calls,3);
  assert.equal(ledger.get(receipt.id).effects.length,2);
  assert.ok(ledger.get(receipt.id).effects.every(e=>e.interaction_id===receipt.id));
});
test('effects retries have bounded exponential backoff and interrupted reads become due after reload', async () => {
  const storage=memory();let time=1;const ledger=createInteractions({storage,now:()=>time});
  const receipt=ledger.accept({command:command()});
  const reconcile=createEffectReconciler({interactions:ledger,read:async()=>{throw new Error('offline');},
    diagnostic:()=>{},now:()=>time,retryMs:10,maxRetryMs:25});
  for(const wait of [10,20,25,25]) {
    await assert.rejects(reconcile(receipt.id));
    assert.equal(ledger.get(receipt.id).effect_sync.next_attempt_at,time+wait);
    time+=wait;
  }
  ledger.effectStatus(receipt.id,{phase:'syncing',next_attempt_at:time+1000});
  const restored=createInteractions({storage,now:()=>time}).get(receipt.id);
  assert.equal(restored.effect_sync.phase,'pending');
  assert.equal(restored.effect_sync.next_attempt_at,0);
});
test('unmeasured effects responses do not turn an unavailable probe into zero changes', async () => {
  const ledger=createInteractions();const receipt=ledger.accept({command:command()});
  const reconcile=createEffectReconciler({interactions:ledger,
    read:async()=>({measured:false,n_considered:0,effects:[],why_unmeasured:'Database unavailable'}),diagnostic:()=>{}});
  await assert.rejects(reconcile(receipt.id),/Database unavailable/);
  assert.equal(ledger.get(receipt.id).effect_sync.phase,'failed');
  assert.equal(ledger.get(receipt.id).effect_sync.measured,false);
});
test('lagged stream requests full refresh and messages invalidate history', async () => {
  assert.deepEqual(invalidationKeys({keys:['board']}),['board']);
  assert.deepEqual(invalidationKeys({keys:['messages']}),['messages','history']);
  let refreshed=0;const invalidated=[];
  const sync=createSync({query:{invalidate:async keys=>invalidated.push(keys)},fetchSync:async()=>({rev:42,full_sync_required:true}),refresh:async()=>{refreshed++;}});
  await Promise.all([sync.catchUp(),sync.catchUp()]);
  assert.equal(refreshed,1); assert.equal(sync.revision(),42);
  assert.ok(invalidated[0].includes('sessions'));
});
test('upload machine cannot complete before sending, cannot requeue after completion', () => {
  const ledger=createInteractions();const r=ledger.accept({command:command()});const actor=uploadActor(ledger,r.id);
  actor.send({type:'COMPLETE'});assert.equal(ledger.get(r.id).phase,'accepted');
  for(const type of ['STORE','SEND','OFFLINE','SEND','WAIT','COMPLETE']) actor.send({type});
  assert.equal(ledger.get(r.id).phase,'applied');
  actor.send({type:'OFFLINE'});assert.equal(ledger.get(r.id).phase,'applied');actor.stop();
});
test('local store selection ignores unrelated ephemeral changes',()=>{
  const store=createStore({tab:'workers',open:false});const values=[];
  const unsub=store.select(s=>s.tab,value=>values.push(value));
  store.setState({open:true});store.setState({tab:'board'});unsub();store.setState({tab:'files'});
  assert.deepEqual(values,['board']);
});

test('AG-UI projection requires chat identities and retains richer domain events as CUSTOM',()=>{
  assert.deepEqual(projectEvent({kind:'message.delta',payload:{messageId:'m1',delta:'hello'}}),{type:'TEXT_MESSAGE_CONTENT',messageId:'m1',delta:'hello'});
  assert.equal(projectEvent({kind:'run.started',payload:{}}).type,'CUSTOM');
  assert.equal(projectEvent({kind:'task.blocked',payload:{interaction_id:'int_1'}}).value.interaction_id,'int_1');
});

test('control registration finds commands after event housekeeping and guards',()=>{
  const registry={editField:{},sendFromInput:{}};
  assert.equal(actionFromHandler('event.stopPropagation();editField(this)',registry),'editField');
  assert.equal(actionFromHandler('event.preventDefault(); if (valid) sendFromInput("amux")',registry),'sendFromInput');
  assert.equal(actionFromHandler('sendFromInput(decodeURIComponent(name))',registry),'sendFromInput');
});

test('successful reads report loaded without claiming a mutation or inventing effects',async()=>{
  const ledger=createInteractions();
  const receipt=ledger.accept({command:commandFor('GET','/api/board'),request:{method:'GET',path:'/api/board'}});
  await ledger.acknowledge(receipt.id,new Response('[]',{headers:{'content-type':'application/json'}}));
  assert.equal(ledger.get(receipt.id).phase,'reconciled');
  assert.equal(ledger.get(receipt.id).feedback.message,'Loaded');
  assert.deepEqual(ledger.get(receipt.id).effects,[]);
});

test('shipped request stamping replaces correlation headers across replay wrappers',()=>{
  const source=readFileSync(new URL('../crates/amux-dashboard/static/app.js',import.meta.url),'utf8');
  const ast=parse(source,{ecmaVersion:'latest',range:true});
  const fn=ast.body.find(node=>node.type==='FunctionDeclaration' && node.id.name==='_interactionRequestOptions');
  assert.ok(fn);
  const stamp=runInNewContext('('+source.slice(...fn.range)+')',{Headers});
  const receipt={id:'int_same',command:{kind:'board.patch'}};
  let options={headers:{'X-Amux-Interaction-Id':'int_old','x-amux-command-kind':'old','Authorization':'Bearer test'}};
  for(let n=0;n<3;n++) options=stamp(receipt,options);
  const headers=new Headers(options.headers);
  assert.equal(headers.get('X-Amux-Interaction-Id'),'int_same');
  assert.equal(headers.get('X-Amux-Command-Kind'),'board.patch');
  assert.equal(headers.get('Authorization'),'Bearer test');
});
