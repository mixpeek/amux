import { test, expect } from '@playwright/test';
import {boot,auth,checkpoint} from './evidence';
test('LC-SEMANTIC-INTAKE: real model appends paraphrases, updates refinements, and preserves distinct outcomes',async({page,request},info)=>{
  test.setTimeout(360_000);expect(process.env.AMUX_LIFECYCLE_LAB_ACK).toBe('dedicated-test-instance');
  await boot(page);const headers=await auth(page);const run=`intake-${Date.now()}`;const created=new Set<string>();const receipts:any[]=[];
  const add=async(title:string,desc:string)=>{const r=await request.post('/api/board',{headers,timeout:150_000,data:{title,desc,type:'chore',owner_type:'human',status:'backlog'}});expect(r.ok(),await r.text()).toBe(true);const c=await r.json();created.add(c.id);receipts.push(c);return c;};
  try {
    const a=await add(`${run}: Normalize invoice money into cents`,`${run} billing importer: parse the uploaded invoice CSV, represent USD amounts as integer cents, and reject malformed monetary strings. Produce normalized invoices.json. Scope: USD only.`);
    const b=await add(`${run}: Convert invoice currency values to integer minor units`,`${run} same billing importer and same invoices.json: avoid floating-point totals by turning uploaded CSV dollar amounts into whole cents. Reject nonnumeric monetary input. This restates the existing normalization work, not an additional output.`);
    expect(b.id).toBe(a.id);expect(b.intake.action).toBe('append');expect(b.intake.comparison.measured).toBe(true);
    const c=await add(`${run}: Refine currency normalization to support EUR too`,`${run} update the same billing importer and invoices.json requirements: replace the USD-only restriction with support for USD and EUR, keeping integer cents and malformed-input rejection. This refines the existing task.`);
    expect(c.id).toBe(a.id);expect(c.intake.action).toBe('update');expect(c.intake.comparison.measured).toBe(true);expect(c.desc).toContain('USD only');expect(c.desc).toContain('support for USD and EUR');
    const d=await add(`${run}: Build dark-mode controls for the invoice dashboard`,`${run} independent UI deliverable: implement a persistent dark/light theme toggle for the invoice dashboard. No changes to normalization or invoices.json. Separate acceptance: theme persists across reload and is keyboard accessible.`);
    expect(d.id).not.toBe(a.id);expect(d.intake.action).toBe('create');expect(d.intake.comparison.measured).toBe(true);
    const e=await add(`${run}: Normalize payroll currency values`,`${run} independent payroll application: convert employee salary CSV fields to integer cents and produce payroll.json. This is a different dataset and output from the billing invoice importer; do not change invoice work.`);
    expect(e.id).not.toBe(a.id);expect(e.id).not.toBe(d.id);expect(e.intake.action).toBe('create');
    expect(created.size).toBe(3);
    await page.goto('/#issue='+a.id);await expect(page.locator('#bd-key')).toHaveText(a.id);await expect(page.locator('#bd-preview')).toContainText('support for USD and EUR');await checkpoint(page,info,'semantic-refined-task');
  } finally {await info.attach('semantic-intake-proof',{body:JSON.stringify(receipts,null,2),contentType:'application/json'});for(const id of created)await request.delete('/api/board/'+id,{headers});}
});
