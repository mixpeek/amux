#!/usr/bin/env node
// A real translated tmux launch and provider child; private API + shipped CLI.
import fs from 'node:fs';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {startAmux,waitFor} from './harness.mjs';
if(process.platform!=='darwin'||execFileSync('/usr/sbin/sysctl',['-n','hw.optional.arm64'],{encoding:'utf8'}).trim()!=='1'){
 console.log(JSON.stringify({verdict:'native_launch_unmeasured',measured:false,n_considered:0,why_unmeasured:'requires Apple Silicon and Rosetta'}));process.exit(0);
}
const amux=await startAmux({binary:process.env.AMUX_CHAOS_BINARY,env:{AMUX_NATIVE_ARCH:'1',FAKE_CLAUDE_PROBE_CHILD_ARCH:'1'}});
const checks=[];const check=(name,ok,detail)=>{checks.push({name,ok:!!ok,detail});if(!ok)throw Error(name+': '+JSON.stringify(detail));};
const repo=path.resolve(path.dirname(new URL(import.meta.url).pathname),'../..');
const cliBin=path.join(amux.root,'cli-bin');fs.mkdirSync(cliBin);fs.writeFileSync(path.join(cliBin,'curl'),'#!/bin/sh\nexit 7\n',{mode:0o755});
async function lane(name,transport,expected){
 const dir=path.join(amux.root,name);fs.mkdirSync(dir);
 const created=await amux.req('POST','/api/sessions',{name,dir,start:false});check('private worker created '+name,created.status===201,created.body);
 if(transport==='api'){
  const started=await amux.req('POST',`/api/sessions/${name}/start`);check('API start succeeds '+name,started.status<300,started.body);
 }else{
  execFileSync('bash',[process.env.AMUX_CHAOS_CLI||path.join(repo,'amux'),'start',name,'--detach'],{env:{...amux.env,PATH:cliBin+path.delimiter+amux.env.PATH,AMUX_API:amux.base,AMUX_URL:amux.base,AMUX_API_URL:amux.base,CC_HOME:amux.home},timeout:30000,encoding:'utf8'});
  check('shipped CLI start succeeds '+name,true);
 }
 await waitFor('provider launch '+name,()=>amux.fakeLog().some(r=>r.event==='launch'&&r.cwd===fs.realpathSync(dir)),30000);
 const launch=amux.fakeLog().find(r=>r.event==='launch'&&r.cwd===fs.realpathSync(dir));
 check('actual provider child architecture '+name,launch.child_translated===expected,launch);
}
try{
 await lane('native-api','api','0');
 await lane('native-cli','cli','0');
 await amux.down();amux.env.AMUX_NATIVE_ARCH='0';await amux.up();
 await lane('optout-api','api','1');
 await lane('optout-cli','cli','1');
 check('API native launch emits named signal',fs.readFileSync(amux.serverLog,'utf8').includes('native_provider_launch'));
}catch(e){checks.push({name:'scenario completed',ok:false,detail:String(e.stack||e)});console.error(fs.readFileSync(amux.serverLog,'utf8').slice(-6000));}
finally{await amux.stop();}
const receipt={measured:true,n_considered:checks.length,failed:checks.filter(c=>!c.ok).length,artifacts:amux.root,fixture_boundary:'private real tmux; actual provider child sysctl; shipped API and CLI direct fallback (HTTP blocked) with explicit opt-out',checks};
fs.writeFileSync(path.join(amux.root,'native-launch-receipt.json'),JSON.stringify(receipt,null,2));console.log(JSON.stringify(receipt,null,2));process.exit(receipt.failed?1:0);
