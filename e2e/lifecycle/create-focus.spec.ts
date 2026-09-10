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
