import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
const code=source.slice(source.indexOf('const _workerLifecyclePending ='),source.indexOf('function updatePeekStatus()'));
function badge(worker) {
 const ctx=vm.createContext({esc:String,escJs:String,_agentsChip:()=>'',_waitingTitle:()=>'',_waitingLabel:()=> 'needs input',_idleMovedTitle:()=>'',_idleMovedSuffix:()=>''});
 vm.runInContext(code,ctx);
 return ctx._workerExecutionBadge({name:'test-worker',...worker},{});
}
test('lifecycle overrides provider state and incomplete pause stays visible',()=>{
 assert.match(badge({lifecycle:'paused',running:false,status:'active'}),/>paused</);
 assert.match(badge({lifecycle:'paused',running:true,status:'active'}),/pause incomplete/);
 assert.match(badge({running:false,status:'active'}),/>stopped</);
});
test('every observed runtime state exposes inspectable status evidence',()=>{
 for(const status of ['active','idle','waiting','blocked','error','rate_limited']) {
   const html=badge({running:true,status});
   assert.match(html,/Status evidence for test-worker/);
   assert.match(html,/_openStatusDetail/);
 }
});
test('the Enter chip sends the suggestion if one is showing, else presses Enter',()=>{
 const calls=[];const ctx=vm.createContext({peekSession:'pk',peekQuickKeys:k=>calls.push(['peek-key',k]),doKeys:(n,k)=>calls.push([n,k]),_submitSuggestion:(n,isPeek,fb)=>calls.push(['suggest',n,isPeek,fb])});
 vm.runInContext(source.slice(source.indexOf('function _chipAction('),source.indexOf('function renderChips(')),ctx);
 ctx._chipAction({action:'keys',value:'Enter'},'raw',false);
 ctx._chipAction({action:'keys',value:'Enter'},'',true);
 ctx._chipAction({action:'keys',value:'Up'},'raw',false);
 assert.deepEqual(calls,[['suggest','raw',false,'Enter'],['suggest','pk',true,'Enter'],['raw','Up']]);
});
for (const isolated of [true,false]) test(`empty Send on a${isolated?'n isolated':' normal'} lane asks the server for the suggestion before any bare Enter`,async()=>{
 const run=async reply=>{
  const calls=[];const ctx=vm.createContext({sessions:[{name:'raw',isolated}],API:'',APP_VER:'t',_gridPanes:{},
   showSendingIndicator(){},showToast:m=>calls.push('toast:'+m),amuxTrack(){},_refreshPeekSoon(){},setTimeout(){},
   peekQuickKeys:async k=>{calls.push('peek:'+k);return {accepted:true,effect:'unverified'};},
   doKeys:async(n,k)=>{calls.push(n+':'+k);return {accepted:true,effect:'unverified'};},
   fetch:async(url,o)=>{calls.push('POST '+url+' '+o.body);return {status:200,json:async()=>reply};}});
  vm.runInContext(source.slice(source.indexOf('async function _submitSuggestion('),source.indexOf('function _showSteerPrompt(')),ctx);
  await ctx._submitSuggestion('raw',false);
  return calls;
 };
 const sent=await run({ok:true,message:'sent'});
 assert.deepEqual(sent.slice(0,2),['POST /api/sessions/raw/send {"text":""}','toast:Sent suggestion']);
 assert.ok(!sent.includes('raw:Enter'),'a submitted suggestion must not also press Enter');
 const none=await run({ok:true,submission:'no_effect',message:'no suggestion found'});
 assert.deepEqual(none.slice(0,2),['POST /api/sessions/raw/send {"text":""}','raw:Enter']);
});
test('the worker-details header pill is the status-evidence button, with no separate info icon',()=>{
 const ctx=vm.createContext({esc:String,escJs:String,_agentsChip:()=>'',_waitingTitle:()=>'',_waitingLabel:()=> 'needs input',_idleMovedTitle:()=>'',_idleMovedSuffix:()=>''});
 vm.runInContext(code,ctx);
 const header=ctx._workerExecutionBadge({name:'test-worker',running:true,status:'idle'},{},{inspect:false});
 assert.doesNotMatch(header,/ⓘ/);
 assert.match(badge({running:true,status:'idle'}),/ⓘ/);
 const peek=source.slice(source.indexOf('function updatePeekStatus()'),source.indexOf('function shellWords('));
 assert.match(peek,/_workerExecutionBadge\(s, runtimeBoard, \{ inspect: false \}\)/);
 assert.match(peek,/el\.onclick = \(\) => _openStatusDetail\(s\.name\)/);
 assert.match(peek,/el\.setAttribute\('role', 'button'\)/);
});
