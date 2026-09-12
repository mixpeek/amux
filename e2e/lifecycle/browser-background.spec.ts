import {test,expect} from '../fixtures';
import {boot,auth} from './evidence';
import {execFileSync} from 'node:child_process';
import {readFile,writeFile} from 'node:fs/promises';
import path from 'node:path';
test('LC-BROWSER-BACKGROUND: real browser does not focus the desktop and expires with saved profile intact',async({page,request},info)=>{
 test.skip(!process.env.AMUX_LIFECYCLE_BROWSER_TTL_S,'Run with AMUX_LIFECYCLE_BROWSER_TTL_S=20; launches one real scratch Chrome');
 test.setTimeout(90_000);await boot(page);const headers=await auth(page);
 const name=`lc-background-${Date.now()}`;let owned=false;let marker='';
 const front=()=>process.platform==='darwin'?JSON.parse(execFileSync('/usr/bin/osascript',['-l','JavaScript','-e','ObjC.import("AppKit"); const app=$.NSWorkspace.sharedWorkspace.frontmostApplication; JSON.stringify({pid:Number(app.processIdentifier),bundle:ObjC.unwrap(app.bundleIdentifier)})'],{encoding:'utf8'})):null;
 const foreground:any[]=[];
 const assertBackground=(pid:number)=>{const observed=front();foreground.push(observed);if(observed)expect(observed.pid,'Amux-owned Chrome must never become the foreground app').not.toBe(pid);};
 const before=front();
 const jobsResponse=await request.get('/api/system-jobs',{headers});expect(jobsResponse.ok()).toBe(true);
 const jobs=(await jobsResponse.json()).jobs;
 expect(jobs.filter((j:any)=>j.spawned&&j.status!=='disabled').map((j:any)=>j.id)).toEqual(['browser-idle-reaper']);
 await info.attach('browser-test-job-isolation',{body:JSON.stringify(jobs),contentType:'application/json'});
 const status=async()=>{const r=await request.get('/api/browser/status',{headers});expect(r.ok()).toBe(true);return r.json();};
 expect((await status()).browsers||[],'scratch server owns no preexisting browser').toHaveLength(0);
 try {
  const profile=await request.post('/api/browser/profile/create',{headers,data:{name}});expect(profile.ok(),await profile.text()).toBe(true);expect((await profile.json()).launched).toBe(false);
  const r=await request.post('/api/browser/start',{headers,data:{profile:name,session:name,url:'about:blank'}});expect(r.ok(),await r.text()).toBe(true);
  const started=await r.json();owned=true;expect(started.headless).toBe(true);expect(execFileSync('/bin/ps',['-p',String(started.pid),'-o','command='],{encoding:'utf8'})).toContain('--headless=new');assertBackground(started.pid);
  marker=path.join(started.user_data_dir,'lifecycle-saved-profile-proof');await writeFile(marker,'Saved profile must survive process expiry');
  const action=await request.post('/api/browser/action',{headers,data:{session:name,action:'eval',script:'document.body.style.cssText="background:white;color:#152238;font:18px system-ui;padding:24px"; document.body.innerHTML="<h1>Background browser verified</h1><button>0</button>"; document.querySelector("button").onclick=function(){this.textContent=String(Number(this.textContent)+1)}; document.querySelector("button").click(); document.querySelector("button").textContent'}});
  expect(action.ok(),await action.text()).toBe(true);expect((await action.json()).result).toBe("1");assertBackground(started.pid);
  const shot=await request.get('/api/browser/screenshot',{headers,params:{session:name}});expect(shot.ok(),await shot.text()).toBe(true);
  const png=await request.get('/api/browser/screenshot/file',{headers,params:{session:name}});expect(png.ok()).toBe(true);
  const bytes=await png.body();expect(bytes.length).toBeGreaterThan(500);await info.attach('background-browser-real-output',{body:bytes,contentType:'image/png'});assertBackground(started.pid);
  await expect.poll(async()=>((await status()).browsers||[]).length,{timeout:40_000,intervals:[500,1000]}).toBe(0);
  await expect.poll(()=>{try{process.kill(started.pid,0);return true;}catch(error:any){if(error.code==='ESRCH')return false;throw error;}},{timeout:10_000,intervals:[250,500]}).toBe(false);owned=false;
  expect(await readFile(marker,'utf8')).toBe('Saved profile must survive process expiry');
  await info.attach('background-browser-proof',{body:JSON.stringify({profile:name,pid:started.pid,headless:started.headless,foregroundBefore:before,foregroundAfter:front(),foregroundSamples:foreground,ttl:Number(process.env.AMUX_LIFECYCLE_BROWSER_TTL_S),idleTTL:0,activityTTL:0,profileSurvived:true,processExited:true}),contentType:'application/json'});
 }finally{if(owned)await request.post('/api/browser/stop',{headers,data:{session:name}});const removed=await request.delete('/api/browser/profile/'+name,{headers});expect(removed.ok(),await removed.text()).toBe(true);}
});
