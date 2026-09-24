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
test('Enter sends a key directly; it cannot enqueue an empty suggested prompt',()=>{
 const calls=[];const ctx=vm.createContext({peekQuickKeys:k=>calls.push(['peek-key',k]),doKeys:(n,k)=>calls.push([n,k]),_submitSuggestion:()=>assert.fail('Enter must not extract a prompt')});
 vm.runInContext(source.slice(source.indexOf('function _chipAction('),source.indexOf('function renderChips(')),ctx);
 ctx._chipAction({action:'keys',value:'Enter'},'raw',false);
 ctx._chipAction({action:'keys',value:'Enter'},'',true);
 assert.deepEqual(calls,[['raw','Enter'],['peek-key','Enter']]);
});
test('raw empty Send remains a literal key without suggestion extraction',async()=>{
 const calls=[];const ctx=vm.createContext({sessions:[{name:'raw',isolated:true}],peekQuickKeys:k=>calls.push(k),doKeys:(n,k)=>calls.push(n+':'+k)});
 vm.runInContext(source.slice(source.indexOf('async function _submitSuggestion('),source.indexOf('function _showSteerPrompt(')),ctx);
 await ctx._submitSuggestion('raw',false);
 assert.deepEqual(calls,['raw:Enter']);
});
