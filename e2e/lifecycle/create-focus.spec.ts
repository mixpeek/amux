import {test,expect} from '../fixtures';
import {boot} from './evidence';
test('LC-CREATE-FOCUS: delayed dialog focus cannot redirect directory text into the worker name',async({page})=>{
  await boot(page);
  await page.clock.install();
  await page.evaluate(()=>{
    (window as any).openCreate();
    const name=document.getElementById('create-name') as HTMLInputElement;
    name.value='lc-focus-worker';
    (document.getElementById('create-dir') as HTMLInputElement).focus();
  });
  await page.clock.fastForward(150);
  await expect(page.locator('#create-dir')).toBeFocused();
  await page.keyboard.type('/tmp/lifecycle-workspace');
  await expect(page.locator('#create-name')).toHaveValue('lc-focus-worker');
  await expect(page.locator('#create-dir')).toHaveValue('/tmp/lifecycle-workspace');
});

test('LC-NAVIGATION: choosing a main tab cancels a delayed restoration of the old worker',async({page})=>{
  await boot(page);
  await page.clock.install();
  await page.evaluate(()=>{
    sessionStorage.setItem('peekState',JSON.stringify({session:'lc-old-worker',tab:'terminal'}));
    (window as any)._restoreScreen();
  });
  await page.locator('#tab-sessions').click();
  await page.clock.fastForward(250);
  await expect(page.locator('#peek-overlay')).not.toHaveClass(/active/);
});

test('LC-BOARD-FILTERS: All resets My tasks and shows human and worker owned work',async({page})=>{
  await page.route(/\/api\/board(?:\?.*)?$/,route=>route.fulfill({json:[
    {id:'LCFILTER-1',title:'Human-owned sample',status:'todo',owner_type:'human',tags:[],created:1},
    {id:'LCFILTER-2',title:'Worker-owned sample',status:'todo',owner_type:'agent',session:'lc-filter-worker',tags:[],created:1},
  ]}));
  await boot(page);
  await page.locator('#tab-board').click();
  await page.locator('.board-quick-filters').getByRole('button',{name:'My tasks',exact:true}).click();
  await expect.poll(()=>page.evaluate(()=>eval('_boardLastVisible').map((c:any)=>c.id))).toEqual(['LCFILTER-1']);
  await page.locator('.board-quick-filters').getByRole('button',{name:'All',exact:true}).click();
  await expect.poll(()=>page.evaluate(()=>eval('_boardLastVisible').map((c:any)=>c.id).sort())).toEqual(['LCFILTER-1','LCFILTER-2']);
});
