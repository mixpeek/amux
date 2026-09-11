import { test, expect } from '../fixtures';
import { boot, auth, checkpoint, deleteOwnedWorkers } from './evidence';

test('LC-LATENCY: rapid sends skip retry backoff and render through lightweight terminal polls', async ({page,request}, info) => {
  test.setTimeout(60_000);
  await boot(page);const headers=await auth(page);
  const name=`lc-latency-${info.project.name}-${Date.now()}`;
  const pressSend=()=>info.project.use.hasTouch
    ? page.locator('#peek-overlay .send-split-main').tap()
    : page.locator('#peek-overlay .send-split-main').click();
  expect((await request.post('/api/sessions',{headers,data:{name,dir:'/tmp'}})).status()).toBe(201);
  let release!:()=>void;const held=new Promise<void>(resolve=>release=resolve);
  const posts:{id:string,at:number}[]=[];const peeks:{live:boolean,at:number}[]=[];
  let frame='Terminal transport fixture ready';
  // Controlled transport boundary: observing input is independent of a slow
  // HTTP acknowledgement. This does not claim a new native model execution.
  await page.route(`**/api/sessions/${name}/send**`,async route=>{
    if(route.request().method()==='GET') return route.fulfill({status:202,json:{ok:true,accepted:false}});
    const body=route.request().postDataJSON();posts.push({id:body.msg_id,at:Date.now()});
    frame='Terminal transport fixture\nReceived input: '+body.text;
    if(posts.length===1) await held;
    await route.fulfill({json:{ok:true,submitted:true}});
  });
  await page.route(`**/api/sessions/${name}/peek?**`,route=>{
    const live=new URL(route.request().url()).searchParams.get('live')==='1';
    peeks.push({live,at:Date.now()});
    return route.fulfill({json:{name,live:frame,...(live?{}:{history:'Earlier terminal output\n'}),pane_cols:80}});
  });
  try {
    await page.reload();
    await page.locator(`.card[data-session="${name}"]`).locator('visible=true').first().locator('.card-menu-btn').click();
    await page.locator('.card-menu.open [data-worker-action="peek-terminal"]').click();
    await expect(page.locator('#peek-body')).toContainText('Terminal transport fixture ready');
    await page.locator('#peek-cmd-input').fill('First rapid message');
    await pressSend();
    await expect(page.locator('#peek-cmd-input')).toHaveValue('');
    await expect.poll(()=>posts.length).toBe(1);
    await expect(page.locator('#peek-body')).toContainText('First rapid message');
    await page.locator('#peek-cmd-input').fill('Second rapid message');
    await pressSend();
    await expect(page.locator('#peek-cmd-input')).toHaveValue('');
    const released=Date.now();release();
    await expect.poll(()=>posts.length,{timeout:750,intervals:[20,40,80]}).toBe(2);
    await expect(page.locator('#peek-body')).toContainText('Second rapid message',{timeout:750});
    const rendered=Date.now();
    expect(posts[0].id).not.toBe(posts[1].id);
    expect(peeks.some(p=>p.live && p.at>=posts[0].at)).toBe(true);
    await info.attach('send-latency-measurement',{contentType:'application/json',body:JSON.stringify({scope:'controlled transport, real UI',next_dispatch_ms:posts[1].at-released,second_input_visible_ms:rendered-posts[1].at,posts:posts.length})});
    await checkpoint(page,info,'rapid-send-terminal');
    await expect.poll(()=>page.evaluate(()=>JSON.parse(localStorage.getItem('amux_offline_queue')||'[]').length)).toBe(0);
  } finally {release();await deleteOwnedWorkers(page,request,headers,[name]);}
});
