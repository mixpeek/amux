import {test,expect} from './fixtures';
import {cleanup} from './teardown';

declare const _chat:any;

// Chat "Read aloud" beside Copy (Ethan, 2026-10-13). /api/tts is stubbed, so no
// engine runs: the stub hands back a short silent WAV that really plays in the
// shared bottom player, which is what drives Stop -> Read aloud on its own.
function silentWav(seconds:number){
 const rate=8000,n=Math.round(rate*seconds),b=Buffer.alloc(44+n);
 b.write('RIFF',0);b.writeUInt32LE(36+n,4);b.write('WAVE',8);b.write('fmt ',12);b.writeUInt32LE(16,16);b.writeUInt16LE(1,20);b.writeUInt16LE(1,22);
 b.writeUInt32LE(rate,24);b.writeUInt32LE(rate,28);b.writeUInt16LE(1,32);b.writeUInt16LE(8,34);b.write('data',36);b.writeUInt32LE(n,40);b.fill(128,44);
 return 'data:audio/wav;base64,'+b.toString('base64');
}
const LONG=silentWav(30),SHORT=silentWav(1.2);
const MSGS=[
 {role:'user',text:'Read me something',turn_id:'t1',ts:1760000000},
 {role:'assistant',text:'First **bold** reply.\n\n```js\nconst secret = 1;\n```\n\nAfter the code.',turn_id:'t1',ts:1760000001,duration_ms:2300,usage:{output_tokens:12}},
 {role:'assistant',text:'Second reply, plain.',turn_id:'t2',ts:1760000002},
];

for(const variant of [{name:'dashboard Chat tab',q:null},{name:'embedded chat',q:''},{name:'split-pane chat',q:'&peekSplitChild=1'}]){
test(`Read aloud sits beside Copy and speaks one reply at a time (${variant.name})`,async({page},info)=>{
 const tts:{text:string}[]=[];let mode:'ok'|'short'|'slow'|'fail'='ok';
 const beacons:any[]=[];
 page.on('request',r=>{if(r.url().endsWith('/api/client-debug')&&r.method()==='POST'){try{const x=r.postDataJSON();if(x.kind==='chat-read-aloud')beacons.push(x);}catch{}}});
 await page.route(/\/api\/sessions(?:\?.*)?$/,r=>r.fulfill({json:[{name:'chat-ra-fixture',provider:'claude',chat_companion:true,running:false,dir:'/tmp'}]}));
 await page.route(/\/api\/tts$/,async r=>{
  tts.push(r.request().postDataJSON());
  if(mode==='fail')return r.fulfill({status:503,json:{error:'No TTS engine: install piper or enable say'}});
  await new Promise(res=>setTimeout(res,mode==='slow'?2500:300));
  return r.fulfill({json:{url:mode==='short'?SHORT:LONG,engine:'say'}}).catch(()=>{});
 });
 try{
  if(variant.q===null){
   await page.goto('/');
   await expect.poll(()=>page.evaluate(()=>typeof (window as any)._chatReadAloud)).toBe('function');
   await page.evaluate(()=>{const w=window as any;w.openPeek('chat-ra-fixture');w.setPeekTab('chat');});
  }else await page.goto('/?peekEmbed=chat-ra-fixture&peekTab=chat'+variant.q);
  await expect(page.locator('#peek-overlay')).toBeVisible();
  await expect.poll(()=>page.evaluate(()=>typeof (window as any)._chatReadAloud)).toBe('function');
  const render=(extra:any[]=[])=>page.evaluate(({msgs})=>{
   const w=window as any;w._chatUnmount();_chat.name=(typeof w._peekChatTarget==='function'&&w._peekChatTarget())||'chat-ra-fixture@chat';_chat.messages=msgs;_chat.streaming=null;
   document.getElementById('peek-body')!.classList.add('peek-chat');w._chatRender();
  },{msgs:[...MSGS,...extra]});
  await render();

  const metas=page.locator('.chat-msg.chat-assistant .chat-meta');
  await expect(metas).toHaveCount(2);
  const read=page.locator('.chat-read-btn');
  await expect(read).toHaveCount(2);
  // Directly after Copy, on every reply.
  for(let i=0;i<2;i++){
   expect(await metas.nth(i).evaluate(m=>{const b=m.querySelector('.chat-read-btn');return b?.previousElementSibling?.classList.contains('chat-copy-btn')&&b.previousElementSibling.textContent;})).toBe('Copy');
  }
  await expect(read.nth(0)).toHaveText('Read aloud');

  // Click: the existing /api/tts path gets the reply's plain text (code skipped).
  await read.nth(0).click();
  await expect.poll(()=>tts.length).toBe(1);
  expect(tts[0].text).toContain('First bold reply.');
  expect(tts[0].text).toContain('After the code.');
  expect(tts[0].text).not.toContain('secret');
  expect(tts[0].text).not.toContain('**');
  await expect(read.nth(0)).toHaveText('Stop');
  await expect(read.nth(1)).toHaveText('Read aloud');
  // Same mechanism as every read-aloud, so it lands in the Read alouds history.
  await expect.poll(()=>page.evaluate(()=>eval('(_raIndex||[]).map(e=>e.text).join("|")'))).toContain('First bold reply.');

  // A re-render (turn boundary with a new message) keeps Stop and does not duplicate.
  await render([{role:'assistant',text:'Third reply.',turn_id:'t3',ts:1760000003}]);
  await expect(read).toHaveCount(3);
  await expect(read.nth(0)).toHaveText('Stop');
  await expect(page.locator('.chat-msg.chat-assistant').nth(0).locator('.chat-read-btn')).toHaveCount(1);

  // Starting another reply stops the first.
  await read.nth(1).click();
  await expect.poll(()=>tts.length).toBe(2);
  expect(tts[1].text).toBe('Second reply, plain.');
  await expect(read.nth(1)).toHaveText('Stop');
  await expect(read.nth(0)).toHaveText('Read aloud');

  // Stop resets the button and pauses the shared player.
  await read.nth(1).click();
  await expect(read.nth(1)).toHaveText('Read aloud');
  await expect.poll(()=>page.evaluate(()=>eval('_abAudio.paused'))).toBe(true);

  // Stop while the clip is still generating cancels it and never plays.
  mode='slow';
  await read.nth(0).click();
  await expect(read.nth(0)).toHaveText('Loading\u2026');
  await read.nth(0).click();
  await expect(read.nth(0)).toHaveText('Read aloud');
  await page.waitForTimeout(3000);
  await expect(read.nth(0)).toHaveText('Read aloud');
  expect(await page.evaluate(()=>eval('_abAudio.paused'))).toBe(true);

  // A clip that plays to the end returns to idle by itself.
  mode='short';
  await read.nth(2).click();
  await expect(read.nth(2)).toHaveText('Stop');
  await expect(read.nth(2)).toHaveText('Read aloud',{timeout:8000});

  // Failure: toast with the reason, a chat-read-aloud beacon, and never stuck.
  mode='fail';
  await read.nth(0).click();
  await expect(page.locator('#toast')).toContainText('No TTS engine');
  await expect(read.nth(0)).toHaveText('Read aloud');
  await expect.poll(()=>beacons.length).toBe(1);
  expect(beacons[0].outcome).toBe('failed');
  expect(beacons[0].error).toContain('No TTS engine');

  // Mobile: touch targets and no horizontal overflow.
  const vw=page.viewportSize()!.width;
  // Touch targets: 44 CSS px. Measured on the element, not the viewport box,
  // because the embedded peek (body.peek-embed) scales its whole page by 0.8
  // on purpose; that zoom is the host layout's, not this button's.
  if(vw<=600){
   for(const b of [read.nth(0),page.locator('.chat-copy-btn:not(.chat-read-btn)').first()]){
    const sz=await b.evaluate(el=>({h:(el as HTMLElement).offsetHeight,w:(el as HTMLElement).offsetWidth}));
    expect(sz.h).toBeGreaterThanOrEqual(44);expect(sz.w).toBeGreaterThanOrEqual(44);
   }
  }
  expect(await page.locator('#peek-body').evaluate(el=>el.scrollWidth-el.clientWidth)).toBeLessThanOrEqual(1);
  for(let i=0;i<3;i++){await expect(read.nth(i)).toBeVisible();const right=await read.nth(i).evaluate(el=>{const r=el.getBoundingClientRect();return r.x+r.width;});expect(right).toBeLessThanOrEqual(vw+1);}
  // Evidence only. A page shot: the chat log can re-render between frames, and an element shot then fails as detached.
  await page.screenshot({path:info.outputPath('chat-read-aloud.png')});
 }finally{await cleanup('stop playback',()=>page.evaluate(()=>{try{eval('_ttsCancel()');}catch{}}),info);}
});
}
