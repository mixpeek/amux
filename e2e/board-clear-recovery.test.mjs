import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import {createQueries} from '../crates/amux-dashboard/static/state/query.mjs';
const source=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
const read=source.slice(source.indexOf('async function fetchBoard() {'),source.indexOf('// timeAgo lives once, further UP'));
const queued=source.slice(source.indexOf('function _isLocallyQueued(r) {'),source.indexOf('// ── Interaction receipts:'));
const clear=source.slice(source.indexOf('async function _fenceBoardMutationReads() {'),source.indexOf('function addBoardStatus() {'));
const turn=()=>new Promise(r=>setImmediate(r));
const deferred=()=>{let resolve;const promise=new Promise(r=>resolve=r);return {promise,resolve};};
function fixture() {
 let server=[{id:'done',status:'done',title:'finished'},{id:'todo',status:'todo'}];
 let hold=false;let pending;const post=deferred();const events=[];
 const ctx=vm.createContext({Response,Set,Map,JSON,console,API:'',_stateQuery:createQueries(),boardItems:server.slice(),boardArchived:[],_boardSnapshotEpoch:0,_boardReadGeneration:0,_boardReadAppliedGeneration:0,_boardViewsLoaded:true,_boardReadError:'',_syncReadError:'',_boardEtag:null,lastBoardJSON:'',lastStatusesJSON:'',sessionGates:{},boardStatuses:[],consecutiveFailures:0,online:true,_boardFullTs:0,_cdcSeq:0,navigator:{onLine:true},amuxTrack:(name,data)=>events.push({name,data}),_mergeArchived:x=>x,_clearDeltaSyncRetry:()=>{},updateConnectionStatus:()=>{},setOnline:()=>{},_cacheBoardJSON:()=>{},saveBoardCache:()=>{},renderBoard:()=>{},_nudgeWorkersOnBoardChange:()=>{},showToast:text=>events.push({name:'toast',text}),_apiErrText:async()=> 'fixture read failed',apiCall:()=>post.promise,
  fetch:async url=>{
   if(url.includes('/api/board?')) {
    const data=JSON.stringify(server);
    if(hold){hold=false;pending=deferred();await pending.promise;}
    return new Response(data,{status:200,headers:{'Content-Type':'application/json'}});
   }
   return new Response(JSON.stringify(url.endsWith('/statuses')?[{id:'done'},{id:'todo'}]:url.includes('/changes')?{cursor:1}:{}));
  }
 });
 vm.runInContext(queued+'\n'+read+'\n'+clear,ctx);
 return {ctx,events,post,hold:()=>{hold=true;},release:()=>pending.resolve(),committed:()=>{server=server.filter(i=>i.status!=='done');post.resolve(new Response(JSON.stringify({archived:1})));}};
}
for(const when of ['before','during'])test(`clear fences a real fetchBoard read started ${when} its POST`,async t=>{
 const f=fixture();t.after(()=>f.ctx._stateQuery.client.clear());let poll;
 if(when==='before'){f.hold();poll=f.ctx.fetchBoard();await turn();}
 const clearing=f.ctx.clearDone();await turn();
 assert.equal(f.ctx.boardItems.some(x=>x.id==='done'),false,'optimistic clear is visible');
 if(when==='during'){f.hold();poll=f.ctx.fetchBoard();await turn();}
 f.committed();f.release();await Promise.all([clearing,poll]);
 assert.equal(f.ctx.boardItems.some(x=>x.id==='done'),false,'late pre-commit body cannot resurrect archived work');
 assert.equal(f.ctx.boardItems.filter(x=>x.id==='todo').length,1);
 assert.ok(f.events.some(x=>x.name==='board_clear_reconciled'));
 f.ctx._stateQuery.client.clear();
});
test('failed clear restores missing cards while preserving a newer same-id update',async t=>{
 const f=fixture();t.after(()=>f.ctx._stateQuery.client.clear());const clearing=f.ctx.clearDone();await turn();
 f.ctx.boardItems.push({id:'done',status:'doing',title:'newer owner update'});
 f.post.resolve(null);await clearing;
 const same=f.ctx.boardItems.filter(x=>x.id==='done');assert.equal(same.length,1);assert.equal(same[0].title,'newer owner update');
 assert.ok(f.events.some(x=>x.name==='board_clear_restored'));f.ctx._stateQuery.client.clear();
});

test('queued offline clear never claims a confirmed server archive',async t=>{
 const f=fixture();t.after(()=>f.ctx._stateQuery.client.clear());const clearing=f.ctx.clearDone();await turn();
 f.post.resolve(new Response(JSON.stringify({queued:true}),{status:202,headers:{'X-Amux-Outbox':'queued'}}));await clearing;
 assert.ok(f.events.some(x=>x.name==='board_clear_queued'));
 assert.ok(f.events.some(x=>x.name==='toast'&&x.text.includes('awaiting server confirmation')));
 assert.equal(f.events.some(x=>x.name==='board_clear_reconciled'),false);
 f.ctx._stateQuery.client.clear();
});
