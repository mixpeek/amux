import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source=fs.readFileSync('crates/amux-dashboard/static/app.js','utf8');
const code=source.slice(source.indexOf('const _restartingSessions ='),source.indexOf('// ── Sending indicator'));
function setup(states){
  let time=0,reads=0;const calls=[];
  const ctx=vm.createContext({API:'',Date:{now:()=>time},AbortController,
    setTimeout:(fn,ms)=>{time+=ms;queueMicrotask(fn);return 1;},clearTimeout:()=>{},
    fetch:async()=>({ok:true,status:200,json:async()=>({running:states[Math.min(reads++,states.length-1)]})}),
    apiCall:async()=>{calls.push('queued stop');return null;},
    fetchSessions:async()=>calls.push('refresh'),doStart:async name=>calls.push('start '+name),
    amuxTrack:(name)=>calls.push(name),showToast:msg=>calls.push(msg)});
  vm.runInContext(code,ctx);return {ctx,calls};
}
test('restart follows queued stop through observed termination',async()=>{
 const {ctx,calls}=setup([true,true,false]);await ctx.doRestart('raw');
 assert.equal(calls.filter(c=>c==='queued stop').length,1);
 assert.equal(calls.filter(c=>c==='start raw').length,1);
 assert.ok(calls.indexOf('queued stop')<calls.indexOf('start raw'));
});
test('restart never starts while stop is unconfirmed',async()=>{
 const {ctx,calls}=setup([true]);await ctx.doRestart('raw');
 assert.equal(calls.includes('start raw'),false);
 assert.ok(calls.some(c=>c.includes("Stop didn't take effect")));
});
test('already stopped restart skips stop and starts directly',async()=>{
 const {ctx,calls}=setup([false]);await ctx.doRestart('raw');
 assert.equal(calls.includes('queued stop'),false);assert.equal(calls.includes('start raw'),true);
});
