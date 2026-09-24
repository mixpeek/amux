import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const source = fs.readFileSync('crates/amux-dashboard/static/app.js', 'utf8');
const code = source.slice(source.indexOf('const _peekRequests ='), source.indexOf('// Split DOM: history'));
function fixture() {
  const calls = [], beacons = [], timers = new Map();
  let clock = 10000, timer = 0, generation = 1;
  const body = {style:{setProperty(){}}, querySelector:()=>null, scrollTop:0, scrollHeight:100, innerHTML:''};
  const status = {textContent:''};
  const ctx = vm.createContext({
    Promise, Map, JSON, AbortController, console:{error(){}}, Date, Math,
    performance:{now:()=>clock}, setTimeout:(fn,ms)=>{timers.set(++timer,{fn,ms}); return timer;}, clearTimeout:id=>timers.delete(id),
    sessions:[{name:'alpha',provider:'codex'}], sessionProvider:s=>s.provider || 'claude',
    peekSession:'alpha', peekSelecting:false, peekSearchQuery:'', API:'', APP_VER:'test',
    _peekAgents:{selected:null}, _peekAgentsLoad(){}, _peekLoadPlan(){}, _peekPlanLast:10000,
    _peekIdentity:name=>({name,generation}), _peekIdentityCurrent:id=>id.name===ctx.peekSession && id.generation===generation,
    _peekIdentityDiscard(){}, _peekHasSelection:()=>false, _peekPollBeacon:(action,name,extra)=>beacons.push({action,name,...extra}),
    _peekGeoHold:0, _peekLastFullMs:0, _peekLastFullAttemptMs:0, _peekEtag:null, _peekLiveEtag:null,
    _peekHistoryRaw:'', _peekHistoryHTML:'', _lastPeekRaw:'', _lastLiveHTML:'', lastPeekHTML:'',
    _peekEarlier:{}, _peekEarlierHTML:()=>'', _trimPeekLiveOverlap:(_history,live)=>live,
    _peekLiveHtml:value=>value, _peekHtml:value=>value, hidePeekLoading(){}, _stopPeekPoll(){},
    _peekPollActive:true, _peekPollInFlight:false, _peekPollAgain:false, _peekFullPending:false,
    _peekScrollLocked:false, _peekFollowBottom:true, _peekBufferedOutput:false, _peekPendingFindScroll:false,
    _isScrolledToBottom:()=>true, _sendingSnapshot:null, _hideScrollLockBadge(){}, _showScrollLockBadge(){},
    _peekReclassifyPrompts(){}, _idb:{set(){},get:async()=>null},
    document:{getElementById:id=>id==='peek-body'?body:id==='peek-status'?status:null},
    applyPeekSearch(){body.innerHTML=ctx.lastPeekHTML;},
    fetch(url,options) {
      let resolve, reject;
      const promise=new Promise((yes,no)=>{resolve=yes;reject=no;});
      options.signal.addEventListener('abort',()=>reject(new Error('aborted')));
      calls.push({url,options,reply(data,httpStatus=200) {
        resolve({ok:httpStatus===200,status:httpStatus,headers:{get:()=> 'etag-'+calls.length},json:async()=>data});
      }});
      return promise;
    },
  });
  vm.runInContext(code,ctx);
  ctx._resetPeekRequests();
  return {ctx,calls,beacons,body,status,timers,advance(ms){clock+=ms;},switchWorker(name){generation++;ctx.peekSession=name;ctx._resetPeekRequests();}};
}
const frame=(live,history)=>({name:'alpha',live,...(history===undefined?{}:{history})});

test('full history cannot block streaming; late history cannot rewind the current frame', async()=>{
  const f=fixture();
  const history=f.ctx.refreshPeek();
  const first=f.ctx.refreshPeek(true);
  assert.equal(f.ctx.refreshPeek(true),first,'duplicate live requests must join');
  assert.equal(f.ctx.refreshPeek(),history,'duplicate history requests must join');
  assert.equal(f.calls.length,2);
  assert.match(f.calls[1].url,/lines=0&live=1&notrim=1/);
  f.calls[1].reply(frame('current frame')); await first;
  const second=f.ctx.refreshPeek(true);
  f.calls[2].reply(frame('newer frame')); await second;
  assert.equal(f.ctx._lastPeekRaw,'newer frame');
  f.calls[0].reply(frame('obsolete frame','saved history\n')); await history;
  assert.equal(f.ctx._lastPeekRaw,'newer frame');
  assert.equal(f.ctx._peekHistoryRaw,'saved history\n');
  assert.equal(f.body.innerHTML,'saved history\nnewer frame');
  assert.ok(f.beacons.some(b=>b.action==='stale-live-suppressed'));
  assert.ok(f.beacons.some(b=>b.action==='first-frame' && b.source==='live'));
});

test('late initial live response cannot rewind a newer full response',async()=>{
  const f=fixture(), live=f.ctx.refreshPeek(true), full=f.ctx.refreshPeek();
  f.calls[1].reply(frame('new output','history\n')); await full;
  f.calls[0].reply(frame('old output')); await live;
  assert.equal(f.body.innerHTML,'history\nnew output');
  assert.equal(f.ctx._peekLiveEtag,null,'a discarded live frame must not acknowledge its ETag');
  const retry=f.ctx.refreshPeek(true);
  assert.equal(f.calls[2].options.headers,undefined,'the next live read must fetch actual bytes');
  f.calls[2].reply(frame('current confirmed output'));await retry;
  assert.equal(f.body.innerHTML,'history\ncurrent confirmed output');
});

test('switching workers aborts pending requests and old cleanup cannot drop new requests',async()=>{
  const f=fixture(), old=f.ctx.refreshPeek(true);
  f.switchWorker('beta');
  assert.equal(f.calls[0].options.signal.aborted,true);
  const fresh=f.ctx.refreshPeek(true);
  await old;
  assert.equal(f.ctx.refreshPeek(true),fresh);
  f.calls[1].reply({name:'beta',live:'beta output'}); await fresh;
  assert.equal(f.body.innerHTML,'beta output');
  assert.equal(f.beacons.filter(b=>b.action==='refresh-failed').length,0);
});

test('stalled live requests have a short deadline and release their slot for recovery',async()=>{
  const f=fixture(), stuck=f.ctx.refreshPeek(true);
  const deadline=[...f.timers.values()].find(t=>t.ms===3000);
  assert.ok(deadline,'live request must not wait 15 seconds');
  // Existing cached content keeps this test on the transport path.
  f.ctx.lastPeekHTML='previous output';
  deadline.fn(); await stuck;
  assert.match(f.status.textContent,/Reconnecting/);
  assert.ok(f.beacons.some(b=>b.action==='refresh-failed' && b.reason==='timeout'));
  const recovered=f.ctx.refreshPeek(true);
  f.calls[1].reply(frame('recovered')); await recovered;
  assert.equal(f.body.innerHTML,'recovered');
  assert.match(f.status.textContent,/Updated/);
});

test('history failure does not mark a healthy live terminal disconnected',async()=>{
  const f=fixture(), history=f.ctx.refreshPeek(), live=f.ctx.refreshPeek(true);
  f.calls[1].reply(frame('healthy')); await live;
  const before=f.status.textContent;
  f.calls[0].reply({},503); await history;
  assert.equal(f.status.textContent,before);
  assert.equal(f.body.innerHTML,'healthy');
  assert.ok(f.beacons.some(b=>b.action==='refresh-failed'));
});

test('a conditional live confirmation protects the displayed frame from an older full response',async()=>{
  const f=fixture();
  let p=f.ctx.refreshPeek(true); f.calls[0].reply(frame('latest')); await p;
  const old=f.ctx.refreshPeek(), live=f.ctx.refreshPeek(true);
  f.calls[2].reply(null,304); await live;
  f.calls[1].reply(frame('stale','history\n')); await old;
  assert.equal(f.body.innerHTML,'history\nlatest');
});

test('selection during fetch keeps the frame and ETag retryable',async()=>{
  const f=fixture(), p=f.ctx.refreshPeek(true);
  f.ctx.peekSelecting=true;
  f.calls[0].reply(frame('selected')); await p;
  assert.equal(f.ctx._peekLiveEtag,null);
  assert.equal(f.ctx._lastPeekRaw,'');
  f.ctx.peekSelecting=false;
  const retry=f.ctx.refreshPeek(true);f.calls[1].reply(frame('selected'));await retry;
  assert.equal(f.body.innerHTML,'selected');
});

test('session updates repaint status immediately and request a coalesced live tick, not full history',()=>{
  const calls=[];
  const ctx=vm.createContext({peekSession:'alpha',document:{hidden:false,getElementById:()=>({classList:{contains:()=>true}})},updatePeekStatus:()=>calls.push('status'),_peekPollNow:()=>calls.push('live')});
  vm.runInContext(source.slice(source.indexOf('function _refreshOpenPeekOnSessions()'),source.indexOf('function fetchSessions()')),ctx);
  ctx._refreshOpenPeekOnSessions();
  assert.deepEqual(calls,['status','live']);
});

test('poll loop keeps ticking with a held history response and coalesces input nudges',async()=>{
  const f=fixture();
  Object.assign(f.ctx,{
    peekTimer:null,_peekPollGen:0,_peekPollSession:'alpha',_peekPrevStatus:'active',_peekUrgentUntil:0,
    _PEEK_HISTORY_REFRESH_MS:30000,_peekUpdateBranch(){},updatePeekStatus(){},_peekPollInterval:()=>350,
    _stopPeekPoll(){f.ctx._peekPollGen++;if(f.ctx.peekTimer)f.timers.delete(f.ctx.peekTimer);f.ctx.peekTimer=null;},
  });
  f.ctx.document.hidden=false;
  f.ctx.sessions[0].status='idle'; // turn-end schedules one full history refresh
  vm.runInContext(source.slice(source.indexOf('function _schedulePeekPoll('),source.indexOf('// Composer drafts live in ONE place')),f.ctx);
  f.ctx._schedulePeekPoll(0);
  const tick=f.timers.get(f.ctx.peekTimer).fn();
  assert.equal(f.calls.length,2);
  for(let i=0;i<20;i++) f.ctx._peekPollNow();
  assert.equal(f.calls.length,2,'nudges cannot fork requests');
  f.calls[1].reply(frame('tick one'));await tick;
  assert.equal(f.timers.get(f.ctx.peekTimer).ms,40,'queued input schedules an immediate next live tick');
  const next=f.timers.get(f.ctx.peekTimer).fn();
  assert.equal(f.calls.length,3,'next tick is live while original history is still pending');
  assert.match(f.calls[2].url,/live=1/);
  f.calls[2].reply(frame('tick two'));await next;
  assert.equal(f.body.innerHTML,'tick two');
  f.calls[0].reply(frame('old','history\n'));
  await f.ctx.refreshPeek();
  assert.equal(f.body.innerHTML,'history\ntick two');
});

test('sending feedback uses the live loop instead of spawning history timers',()=>{
  const delays=[], calls=[];
  const ctx=vm.createContext({lastPeekHTML:'current',_sendingSnapshot:null,_sendingTimer:null,
    document:{querySelector:()=>({parentElement:{}}),getElementById:id=>id==='peek-overlay'?{classList:{contains:()=>true}}:{style:{}}},
    _peekKickFast:()=>calls.push('live'),clearTimeout(){},clearSendingIndicator(){},setTimeout:(_fn,ms)=>delays.push(ms),
  });
  vm.runInContext(source.slice(source.indexOf('function showSendingIndicator()'),source.indexOf('function clearSendingIndicator()')),ctx);
  ctx.showSendingIndicator();
  assert.deepEqual(calls,['live']);
  assert.deepEqual(delays,[15000],'only the indicator safety timer remains');
});


test('idle attach input has a half-second polling budget without speeding offline requests',()=>{
 let now=10000;
 const ctx=vm.createContext({online:true,performance:{now:()=>now},_peekUrgentUntil:0,_peekLastChangeMs:0});
 vm.runInContext(source.slice(source.indexOf('function _peekPollInterval()'),source.indexOf('// After a send/keystroke')),ctx);
 assert.equal(ctx._peekPollInterval(),500,'native attach has no browser send nudge');
 ctx._peekLastChangeMs=now;assert.equal(ctx._peekPollInterval(),250);
 ctx._peekUrgentUntil=now+1500;assert.equal(ctx._peekPollInterval(),100);
 ctx.online=false;assert.equal(ctx._peekPollInterval(),1500,'an outage must not poll at input-burst cadence');
 now+=2000;ctx.online=true;assert.equal(ctx._peekPollInterval(),250);
 now+=1000;assert.equal(ctx._peekPollInterval(),500);
});
