import {test,expect} from './fixtures';
import {cleanup} from './teardown';

declare const _chat:any;

// Owner, 2026-11-23, on a worker's Chat tab: "when I click jump to bottom it
// still doesnt do it". While a reply streams in, the page grows between the jump
// and the scroll events the jump causes. The chat's Latest jump scrolled
// smoothly, every frame of that read "not at the bottom", following switched
// off and the view ended 2,400 px behind after 2 s. The peek's Jump to bottom
// left the chat's follow state alone, so a trackpad's momentum after the click
// did the same. Both now go through one jump that lands and holds while the
// scrolling settles.
const MSGS=Array.from({length:40},(_,i)=>({role:i%2?'assistant':'user',text:`Message ${i}\n\n`+'A line of earlier conversation. '.repeat(12),turn_id:'t'+i,ts:1760000000+i}));

async function open(page:any){
 await page.route(/\/api\/sessions(?:\?.*)?$/,(r:any)=>r.fulfill({json:[{name:'chat-jump-fixture',provider:'claude',chat_companion:true,running:true,status:'active',dir:'/tmp'}]}));
 await page.goto('/');
 await expect.poll(()=>page.evaluate(()=>typeof (window as any)._chatJumpToLatest)).toBe('function');
 await page.evaluate((msgs:any)=>{
  const w=window as any;w.openPeek('chat-jump-fixture');w.setPeekTab('chat');
  w._chatUnmount();w._chatMount('chat-jump-fixture@chat');
  // Paint a fixed conversation; the mount's own load is not what this tests.
  _chat.messages=msgs;_chat.streaming=null;w._chatRender();
 },MSGS);
 await expect(page.locator('#peek-body.peek-chat .chat-msg').first()).toBeVisible();
 // A reply streaming in: the page grows every 40 ms under the chat's own rule.
 await page.evaluate(()=>{
  const w=window as any;const b=document.getElementById('peek-body')!;
  w.__stream=setInterval(()=>{
   const d=document.createElement('div');d.style.height='24px';d.textContent='streamed '+Date.now();
   const msgs=b.querySelectorAll('.chat-msg');(msgs[msgs.length-1]||b).appendChild(d);
   if(_chat.stick)b.scrollTop=b.scrollHeight;
   w._peekScrollAffordance();
  },40);
 });
}
const gap=(page:any)=>page.evaluate(()=>{const b=document.getElementById('peek-body')!;return Math.round(b.scrollHeight-b.scrollTop-b.clientHeight);});
async function readEarlier(page:any){
 // The handler runs in the same task as the scroll, so a repaint cannot slip
 // between them and read the old position.
 await page.evaluate(()=>{const b=document.getElementById('peek-body')!;b.dispatchEvent(new WheelEvent('wheel',{deltaY:-600,bubbles:true}));b.scrollTop-=1200;_chat.onScroll();});
 await expect.poll(()=>page.evaluate(()=>_chat.stick)).toBe(false);
 await page.waitForTimeout(300);
 expect(await gap(page)).toBeGreaterThan(400);
}

test('the chat Latest jump reaches the bottom of a streaming reply and stays there',async({page},info)=>{
 await open(page);
 try{
  await readEarlier(page);
  await page.evaluate(()=>(window as any)._chatJumpToLatest());
  await page.waitForTimeout(1500);
  expect(await gap(page),'at the bottom 1.5 s later, while lines keep arriving').toBeLessThan(80);
  expect(await page.evaluate(()=>_chat.stick)).toBe(true);
 }finally{await cleanup('stop the simulated stream',()=>page.evaluate(()=>clearInterval((window as any).__stream)),info);}
});

test('the peek Jump to bottom on a Chat tab reaches the bottom and survives a momentum scroll',async({page},info)=>{
 await open(page);
 try{
  await readEarlier(page);
  const badge=page.locator('#peek-body .scroll-lock-badge');
  await expect(badge).toBeVisible();
  await badge.click();
  // A trackpad keeps scrolling up a little after the click.
  for(let i=0;i<4;i++){await page.evaluate(()=>{document.getElementById('peek-body')!.scrollTop-=60;});await page.waitForTimeout(60);}
  await page.waitForTimeout(1500);
  expect(await gap(page),'at the bottom, following the stream').toBeLessThan(80);
  expect(await page.evaluate(()=>_chat.stick)).toBe(true);
 }finally{await cleanup('stop the simulated stream',()=>page.evaluate(()=>clearInterval((window as any).__stream)),info);}
});

test('a reader who grabs the view after a jump can scroll up again',async({page},info)=>{
 await open(page);
 try{
  await readEarlier(page);
  await page.evaluate(()=>(window as any)._chatJumpToLatest());
  // The reader takes over between two streamed lines.
  await page.evaluate(()=>clearInterval((window as any).__stream));
  // A press on the view ends the hold; the scroll it starts is the reader's.
  // The scroll handler runs in the same task, so no other repaint can come
  // between the scroll and its reading.
  const r=await page.evaluate(()=>{const b=document.getElementById('peek-body')!;
   const heldBefore=_chat.holdUntil>Date.now();
   b.dispatchEvent(new PointerEvent('pointerdown',{bubbles:true}));
   b.scrollTop-=1200;_chat.onScroll();
   return {heldBefore,held:_chat.holdUntil>Date.now(),stick:_chat.stick,gap:Math.round(b.scrollHeight-b.scrollTop-b.clientHeight)};});
  expect(r.heldBefore,'the jump was holding the bottom').toBe(true);
  expect(r).toMatchObject({held:false,stick:false});
  expect(r.gap).toBeGreaterThan(400);
 }finally{await cleanup('stop the simulated stream',()=>page.evaluate(()=>clearInterval((window as any).__stream)),info);}
});
