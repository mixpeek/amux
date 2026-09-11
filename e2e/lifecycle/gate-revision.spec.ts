import {test,expect} from '../fixtures';
import {boot,auth,checkpoint} from './evidence';
test('LC-GATE-REVISION: edit criteria on a verified task, inspect stale evidence, and explicitly recheck the current gate',async({page,request},info)=>{
  await boot(page);const headers=await auth(page);
  const first=['Invoice total is independently reproduced','Malformed input is rejected'];
  const second=[...first,'Duplicate invoice IDs are rejected'];
  const r=await request.post('/api/board',{headers,data:{title:'Gate revision acceptance '+info.project.name,type:'chore',gate:first}});
  expect(r.ok()).toBe(true);const card=await r.json();const url='/api/board/'+card.id;
  try {
    const checked=await request.patch(url,{headers,data:{evidence:'Independent fixture check passed before the criteria amendment.'}});expect(checked.ok(),await checked.text()).toBe(true);
    await page.goto('/#issue='+card.id);
    await expect(page.locator('#board-detail-overlay')).toHaveClass(/active/);
    await page.locator('#bd-status-select').selectOption('verified');
    await page.locator('#bd-status-row').getByRole('button',{name:'Move',exact:true}).click();
    await expect(page.locator('#_gate-list')).toContainText(first[0]);
    for(const box of await page.locator('#_gate-list input').all())await box.check();
    const moved=page.waitForResponse(r=>r.url().endsWith(url)&&r.request().method()==='PATCH');
    await page.locator('#_gate-ok').click();expect((await moved).ok()).toBe(true);
    await page.reload();
    await expect(page.locator('.bd-verification-section')).toContainText('Verified against recorded criteria');
    await page.locator('#bd-tab-edit').click();await page.locator('#bd-gate').fill(second.join('\n'));
    await page.locator('#bd-edit-footer').getByRole('button',{name:'Save',exact:true}).click();await expect(page.locator('#bd-save-status')).toHaveText('Saved');
    await page.reload();await expect(page.locator('.bd-verification-section')).toContainText('Criteria changed');
    await expect(page.locator('.bd-evidence-section')).toContainText('Independent fixture check passed');
    await checkpoint(page,info,'verified-under-older-gate');
    const stale=await request.patch(url,{headers,data:{status:'verified',reverify:true,gate_checked:first}});expect(stale.status()).toBe(409);
    await page.getByRole('button',{name:'Recheck current gate',exact:true}).click();
    await expect(page.locator('#_gate-list')).toContainText('Duplicate invoice IDs are rejected');
    for(const box of await page.locator('#_gate-list input').all())await box.check();
    const applied=page.waitForResponse(r=>r.url().endsWith(url)&&r.request().method()==='PATCH');await page.locator('#_gate-ok').click();expect((await applied).ok()).toBe(true);
    await expect(page.locator('.bd-verification-section')).toContainText('Verified against recorded criteria');
    await expect(page.getByRole('button',{name:'Recheck current gate',exact:true})).toHaveCount(0);
    await checkpoint(page,info,'verified-under-current-gate');
    const detail=await (await request.get(url,{headers})).json();expect(detail.verification.attempts).toBe(2);expect(detail.verification.gate_snapshot).toEqual(second);
  } finally {await request.delete(url,{headers});}
});
