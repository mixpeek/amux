import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {chromium} from 'playwright';
const app=readFileSync('crates/amux-dashboard/static/app.js','utf8');
const start=app.indexOf('let _bwRouteConfig = {};'),end=app.indexOf('async function _bwAdvanceRouting()',start);
assert(start>0&&end>start);
const config={native_profile:'personal',chrome_profile:'',allow_cua:false,profile_routes:{work:{chrome_profile:'Profile Work',cua_profile:'work',allow_cua:true}}};
async function fixture(run){
 const b=await chromium.launch();const p=await b.newPage();const reports=[];let release,ready;
 try{
  await p.route('http://fixture.local/',r=>r.fulfill({contentType:'text/html',body:'<select id="bw-profile" onchange="_bwRoutingProfileChanged()"><option value="">Default</option><option value="personal">Personal</option><option value="work">Work</option></select><select id="bw-cdp-profile" onchange="_bwRoutingEdited()"></select><select id="bw-cua-profile" onchange="_bwRoutingEdited()"></select><input type="checkbox" id="bw-cua-enabled" onchange="_bwRoutingEdited()"><div id="bw-routing-status"></div>'}));
  await p.route('**/api/client-debug',async r=>{reports.push(r.request().postDataJSON());await r.fulfill({json:{ok:true}});});
  await p.route('**/api/browser/routing/config',async r=>{
   if(r.request().method()==='POST'){const c=r.request().postDataJSON();return r.fulfill({json:{config:{...config,...c}}});}
   const snapshot={config,chrome_profiles:[{name:'Profile Work',on_disk:true}]};
   if(ready){ready();await new Promise(resolve=>release=resolve);}
   await r.fulfill({json:snapshot});
  });
  await p.goto('http://fixture.local/');await p.addScriptTag({content:"const APP_VER='test';\n"+app.slice(start,end)});await p.evaluate(()=>_bwLoadRouting());
  // Return an object to avoid awaiting the deliberately held evaluation.
  await run({p,reports,hold:async()=>{const read=new Promise(r=>ready=r);const pending=p.evaluate(()=>_bwLoadRouting());await read;return {pending};},release:()=>{ready=null;release();}});
 }finally{await b.close();}
}
test('late saved default preserves explicit Work and logs the discarded selection',()=>fixture(async({p,reports,hold,release})=>{
 const {pending}=await hold();await p.selectOption('#bw-profile','work');release();await pending;
 assert.equal(await p.inputValue('#bw-profile'),'work');assert.equal(await p.inputValue('#bw-cdp-profile'),'Profile Work');assert(await p.locator('#bw-cua-enabled').isChecked());
 await p.waitForFunction(()=>document.querySelector('#bw-routing-status').textContent.includes('Profile Work'));
 assert(reports.some(r=>r.reason==='owner_selected_profile_during_load'&&r.measured&&r.n_considered===1));
}));
test('older GET cannot replace the newer owner save',()=>fixture(async({p,reports,hold,release})=>{
 const {pending}=await hold();await p.selectOption('#bw-profile','work');await p.evaluate(()=>_bwSaveRouting());release();await pending;
 assert.equal(await p.evaluate(()=>_bwRouteConfig.native_profile),'work');assert.equal(await p.inputValue('#bw-profile'),'work');assert(reports.some(r=>r.reason==='superseded_by_newer_load_or_edit'));
}));
test('late discovery preserves unsaved fallback edits and explicit default selection',()=>fixture(async({p,hold,release})=>{
 await p.selectOption('#bw-profile','work');const {pending}=await hold();await p.locator('#bw-cua-enabled').uncheck();release();await pending;assert(!(await p.locator('#bw-cua-enabled').isChecked()));
 const next=await hold();await p.selectOption('#bw-profile','');release();await next.pending;assert.equal(await p.inputValue('#bw-profile'),'');
}));

test('reopening route settings retains an existing explicit profile',()=>fixture(async({p})=>{
 await p.selectOption('#bw-profile','work');await p.evaluate(()=>_bwLoadRouting());
 assert.equal(await p.inputValue('#bw-profile'),'work');assert.equal(await p.inputValue('#bw-cdp-profile'),'Profile Work');
}));
