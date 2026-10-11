import assert from 'node:assert/strict';
import {chromium} from 'playwright';
import {createServer} from 'node:http';
import {mkdirSync,writeFileSync} from 'node:fs';
import {join} from 'node:path';
import {api} from '../scripts/browser-route-driver.mjs';
const base=process.env.AMUX_ROUTING_E2E_BASE,home=process.env.AMUX_ROUTING_E2E_HOME,out=process.env.AMUX_ROUTING_EVIDENCE;
assert(base&&home&&out);mkdirSync(out,{recursive:true});
const checks=[];const prove=(s,c)=>{assert(c,s);checks.push(s);console.log('PASS '+s);};
const chromeProfile=process.env.AMUX_ROUTING_UI_CHROME_PROFILE||'Profile 14';
const workIdentity=process.env.AMUX_ROUTING_UI_WORK_IDENTITY||'work@example.test';
for(const [name,identity,role] of [['routing-work',workIdentity,'primary'],['routing-personal','personal@example.test','personal'],['routing-qa',workIdentity,'test']]){
 mkdirSync(join(home,'playwright-auth','profiles',name),{recursive:true});
 await api(base,'/api/browser/profile/meta','POST',{name,identity,role,label:role==='primary'?'Company work account':role==='personal'?'Personal events only':'Synthetic QA only',domains:['example.com']});
}
const profiles=await api(base,'/api/browser/profiles');
prove('profile discovery gives workers semantic cards',profiles.profiles.find(p=>p.name==='routing-work')?.selection?.use_for==='Company work account');
const picked=await api(base,'/api/browser/profile-for?site=example.com&identity=personal%40example.test&role=personal');
prove('identity and role choose personal profile',picked.profile==='routing-personal'&&picked.selection.identity==='personal@example.test');
const normal=await api(base,'/api/browser/profile-for?site=example.com&identity='+encodeURIComponent(workIdentity));
prove('QA excluded from ordinary recommendations',normal.choices.every(p=>p.role!=='test'));
let mismatch=false;try{await api(base,'/api/browser/profile-for?site=example.com&identity=missing%40example.test');}catch(e){mismatch=e.status===404;}
prove('unknown identity refuses arbitrary account fallback',mismatch);
let forbidden=false;try{await api(base,'/api/browser/routing/config','POST',{native_profile:'routing-work',chrome_profile:'Profile 14',cua_profile:'',allow_cua:false},'routing-worker');}catch(e){forbidden=e.status===403;}
prove('worker cannot change owner fallback configuration',forbidden);
const fixture=createServer((q,s)=>s.end('<title>Browser UI proof</title><h1>Real Browser tab proof</h1><input aria-label="Proof"><button>Submit</button>'));
await new Promise(r=>fixture.listen(0,'127.0.0.1',r));
const browser=await chromium.launch({headless:true});
const context=await browser.newContext({ignoreHTTPSErrors:true,viewport:{width:1280,height:900}});
const p=await context.newPage();
try{
 await p.goto(base+'/?view=browser');if(await p.getByText('Skip',{exact:true}).isVisible())await p.getByText('Skip',{exact:true}).click();await p.waitForFunction(()=>typeof window.switchView==='function');await p.evaluate(()=>window.switchView('browser'));
 await p.locator('#bw-routing summary').click();await p.waitForFunction(value=>Array.from(document.querySelector('#bw-cdp-profile').options).some(o=>o.value===value),chromeProfile);
 await p.selectOption('#bw-profile','routing-work');await p.selectOption('#bw-cdp-profile',chromeProfile);await p.selectOption('#bw-cua-profile','routing-work');await p.locator('#bw-cua-enabled').check();
 await p.locator('#bw-routing').getByRole('button',{name:'Save route',exact:true}).click();await p.waitForFunction(()=>document.querySelector('#bw-routing-status').textContent.startsWith('Saved:'));
 const config=await api(base,'/api/browser/routing/config');prove('Browser tab saves selected native/CDP/CUA profiles',config.config.native_profile==='routing-work'&&config.config.chrome_profile===chromeProfile&&config.config.cua_profile==='routing-work'&&config.config.allow_cua);
 await p.reload();await p.waitForFunction(value=>document.querySelector('#bw-profile')?.value==='routing-work'&&document.querySelector('#bw-cdp-profile')?.value===value,chromeProfile);
 if(!(await p.locator('#bw-routing').evaluate(e=>e.open)))await p.locator('#bw-routing summary').click();
 prove('Browser tab reload restores saved profile and fallback choices',await p.inputValue('#bw-cdp-profile')===chromeProfile&&await p.inputValue('#bw-cua-profile')==='routing-work'&&await p.locator('#bw-cua-enabled').isChecked());
 await p.selectOption('#bw-profile','routing-personal');
 prove('unconfigured profile does not inherit the previous account fallback',await p.inputValue('#bw-cdp-profile')===''&&!(await p.locator('#bw-cua-enabled').isChecked()));
 await p.selectOption('#bw-cua-profile','routing-personal');
 await p.locator('#bw-routing').getByRole('button',{name:'Save route',exact:true}).click();await p.waitForFunction(()=>document.querySelector('#bw-routing-status').textContent.startsWith('Saved:'));
 await p.selectOption('#bw-profile','routing-work');
 prove('selecting the work profile restores its saved fallback',await p.inputValue('#bw-cdp-profile')===chromeProfile&&await p.inputValue('#bw-cua-profile')==='routing-work'&&await p.locator('#bw-cua-enabled').isChecked());
 const retained=await api(base,'/api/browser/routing/config');prove('owner choices survive saving a second profile',retained.config.profile_routes['routing-work'].chrome_profile===chromeProfile&&retained.config.profile_routes['routing-personal'].chrome_profile==='');

 // Hold a real saved-default response behind explicit selection and a newer
 // owner write. This reproduced a Personal start and missing CDP on the old UI.
 let releaseDiscovery, discoveryRead, discoveryFinished, discoveryTaken=false;
 const discoveryHeld=new Promise(r=>releaseDiscovery=r);
 const discoveryReady=new Promise(r=>discoveryRead=r);
 const discoveryDone=new Promise(r=>discoveryFinished=r);
 const holdDiscovery=async route=>{
  if(route.request().method()!=='GET'||discoveryTaken)return route.continue();
  discoveryTaken=true;
  const response=await route.fetch();discoveryRead();await discoveryHeld;await route.fulfill({response});discoveryFinished();
 };
 await p.route('**/api/browser/routing/config',holdDiscovery);
 const lateDiscovery=p.evaluate(()=>window._bwLoadRouting());await discoveryReady;
 await p.selectOption('#bw-profile','routing-personal');await p.selectOption('#bw-profile','routing-work');
 releaseDiscovery();await lateDiscovery;await discoveryDone;await p.unroute('**/api/browser/routing/config',holdDiscovery);
 prove('late discovery preserves explicit Work selection and its matching fallback',await p.inputValue('#bw-profile')==='routing-work'&&await p.inputValue('#bw-cdp-profile')===chromeProfile&&await p.locator('#bw-cua-enabled').isChecked());
 let releaseOld, oldRead, oldFinished, oldTaken=false;const oldHeld=new Promise(r=>releaseOld=r);const oldReady=new Promise(r=>oldRead=r);const oldDone=new Promise(r=>oldFinished=r);
 const holdOld=async route=>{if(route.request().method()!=='GET'||oldTaken)return route.continue();oldTaken=true;const response=await route.fetch();oldRead();await oldHeld;await route.fulfill({response});oldFinished();};
 await p.route('**/api/browser/routing/config',holdOld);
 const oldDiscovery=p.evaluate(()=>window._bwLoadRouting());await oldReady;
 await p.evaluate(()=>window._bwSaveRouting());releaseOld();await oldDiscovery;await oldDone;await p.unroute('**/api/browser/routing/config',holdOld);
 prove('late discovery cannot replace a newer owner route save',await p.evaluate(()=>_bwRouteConfig.native_profile==='routing-work')&&await p.inputValue('#bw-profile')==='routing-work');

 await p.fill('#bw-url',`http://127.0.0.1:${fixture.address().port}`);await p.evaluate(()=>window._bwGo());
 await p.waitForFunction(()=>document.querySelector('#bw-img')?.naturalWidth>0,{},{timeout:45000});prove('Browser tab displays a real browser frame',await p.locator('#bw-img').evaluate(e=>e.naturalWidth>0));
 const sessionBefore=await p.evaluate(()=>_bwSession);
 const native=await api(base,'/api/browser/routing/request','POST',{verb:'status',session:sessionBefore,body:{}},sessionBefore);
 prove('Browser tab starts the explicitly selected Work profile',native.route.backend==='amux'&&native.route.selected_profile==='routing-work');
 await p.locator('#bw-routing').getByRole('button',{name:'Try next route',exact:true}).click();
 await p.waitForFunction(()=>document.querySelector('#bw-routing-status').textContent.includes('Active: cdp'));
 const advanced=await api(base,'/api/browser/routing/request','POST',{verb:'status',session:sessionBefore,body:{}},sessionBefore);
 prove('Browser tab advances the unmet goal to the selected work CDP profile',advanced.route.backend==='cdp'&&advanced.route.profile===chromeProfile&&advanced.route.selected_profile==='routing-work'&&advanced.route.attempts.some(a=>a.verdict==='goal_unmet'));
 await p.evaluate(()=>window._bwGo());
 await p.waitForFunction(()=>document.querySelector('#bw-routing-status').textContent.includes('Active: amux'));
 const replaced=await api(base,'/api/browser/routing/request','POST',{verb:'status',session:sessionBefore,body:{}},sessionBefore);
 let oldGone=false;
 for(let i=0;i<20&&!oldGone;i++){
  try{oldGone=!(await api(`http://127.0.0.1:${advanced.route.cdp_port}`,'/json/list')).some(t=>t.id===advanced.route.target);}
  catch(e){if(e.code==='ECONNREFUSED')oldGone=true;else if(e.code!=='ECONNRESET')throw e;}
  if(!oldGone)await new Promise(r=>setTimeout(r,100));
 }
 prove('Browser tab Go replaces CDP with fresh Native and releases its old owned tab',replaced.route.backend==='amux'&&oldGone);
 await p.screenshot({path:join(out,'dashboard-desktop.png'),fullPage:true});
 await p.setViewportSize({width:375,height:812});await p.screenshot({path:join(out,'dashboard-mobile.png'),fullPage:true});
 const bounds=await p.locator('#bw-routing').evaluate(e=>({scroll:e.scrollWidth,width:e.clientWidth}));prove('route settings fit phone width',bounds.scroll<=bounds.width+1);
 const session=await p.evaluate(()=>_bwSession);await api(base,'/api/browser/routing/request','POST',{verb:'stop',session,body:{}},session);
 writeFileSync(join(out,'ui-result.json'),JSON.stringify({verdict:'PASS',measured:true,n_considered:checks.length,checks},null,2));
}catch(e){writeFileSync(join(out,'ui-result.json'),JSON.stringify({verdict:'FAIL',measured:true,n_considered:checks.length,checks,error:e.stack},null,2));throw e;}
finally{const session=await p.evaluate(()=>_bwSession).catch(()=>null);if(session)await api(base,'/api/browser/routing/request','POST',{verb:'stop',session,body:{}},session).catch(()=>{});await browser.close();fixture.close();}
