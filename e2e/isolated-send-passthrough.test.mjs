import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
const code=source.slice(source.indexOf('function _hasSendTimeStamp('),source.indexOf('async function doKeys('));
for (const isolated of [true,false]) test(`real composer send: isolated=${isolated}`,async()=>{
  const sent=[];
  const ctx=vm.createContext({sessions:[{name:'worker',isolated}], API:'',_cloudEmail:null,_localMemberEmail:null,
    crypto:{randomUUID:()=> 'one-send'},AbortSignal, amuxTrack:()=>{},_sendContext:()=>({source:'worker'}),
    _authHeaders:()=>({}),_isLocallyQueued:()=>false,showSendingIndicator:()=>{},
    fetch:async(url,opts)=>{sent.push({url,...JSON.parse(opts.body)});return {ok:true,status:200};}});
  vm.runInContext(code,ctx);
  const literal='[no-board] literal user text\n  preserve spacing and Unicode →';
  assert.equal(await ctx.doSend('worker',literal),'sent');
  assert.equal(sent.length,1);assert.equal(sent[0].record_history,true);
  if(isolated) assert.equal(sent[0].text,literal,'no timestamp, attribution or harness prefix');
  else assert.ok(sent[0].text.endsWith(literal) && sent[0].text!==literal,'managed behavior is unchanged');
  assert.equal(await ctx.doSend('worker','/compact'),'sent');
  assert.equal(sent[1].text,'/compact');
});
test('isolated status never presents an old board claim as current work',()=>{
 const code=source.slice(source.indexOf('function _runtimeBoardPresentation('),source.indexOf('function _runtimeBoardSyncBadge('));
 const ctx=vm.createContext({});vm.runInContext(code,ctx);
 assert.equal(ctx._runtimeBoardPresentation({isolated:true,status:'active',runtime_board:{measured:true,status:'linked',card_id:'OLD-1'}}).cardId,'');
 assert.equal(ctx._runtimeBoardPresentation({isolated:false,status:'active',runtime_board:{measured:true,status:'linked',card_id:'CURRENT-1'}}).cardId,'CURRENT-1');
});
