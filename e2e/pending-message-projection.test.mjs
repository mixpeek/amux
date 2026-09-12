import {test} from 'node:test';import assert from 'node:assert/strict';import fs from 'node:fs';import vm from 'node:vm';
const source=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
const code=source.slice(source.indexOf('function _pendingMessageProjection('),source.indexOf('async function _pendingCancel('));
const ctx=vm.createContext({peekSession:'lane'});vm.runInContext(code,ctx);
const project=(history,pending)=>JSON.parse(JSON.stringify(ctx._pendingMessageProjection(history,pending)));
const pending={id:'q',msg_id:'transport-1',text:'[01:14 PM] same text',ts:100000,attempted:true};
const history={msg_id:'transport-1',text:'same text',time:100000,session:'lane'};
test('one send has one visible pending row, with uncertainty and history retained',()=>{
 const result=project([history],[pending]);assert.equal(result.items.length,0);assert.equal(result.pending.length,1);assert.equal(result.pending[0].attempted,true);assert.equal(result.pending[0].local_notes.length,1);
});
test('identical text from distinct sends remains two messages',()=>{
 const result=project([history,{...history,msg_id:'transport-2'}],[pending]);assert.equal(result.items.length,1);assert.equal(result.items[0].msg_id,'transport-2');assert.equal(result.pending.length,1);
});
test('only unambiguous recent legacy local notes can be grouped; no server history heuristic',()=>{
 const local={...history,msg_id:undefined};assert.equal(project([local],[pending]).pending[0].local_notes.length,1);
 for(const entries of [[{...local,id:'MSG-1'}],[{...local,time:90000}],[local,{...local}], [{...local,session:'other'}]])assert.equal(project(entries,[pending]).items.length,entries.length);
 assert.equal(project([local],[pending,{...pending,id:'q2'}]).items.length,1);
});
test('acknowledgement removes pending display and restores history row',()=>{
 const result=project([history],[]);assert.equal(result.items.length,1);assert.equal(result.pending.length,0);
});
test('server steering replaces only its exact durable outbox identity',()=>{
 const script=source.slice(source.indexOf('function _steerQueueFor('),source.indexOf('function _steerHumanCount('));
 const ctx=vm.createContext({offlineQueue:[{id:'q',url:'/api/sessions/lane/steer',options:{body:JSON.stringify({text:'same',msg_id:'one'})},timestamp:Date.now()}]});vm.runInContext(script,ctx);
 const server={name:'lane',steering:[{id:'steer-server',text:'same',transport_id:'one'}]};
 assert.equal(ctx._steerQueueFor(server).length,1);assert.equal(ctx.offlineQueue.length,1,'projection must not delete durable intent');
 server.steering[0].transport_id='two';assert.equal(ctx._steerQueueFor(server).length,2,'same text from another send remains separate');
});
