import {test,expect} from './fixtures';

// Full shipped Settings/instance/locked-out UI; API fixtures are nonbillable.
// Rust tests exercise the same server routes, session helpers and TLS loader.
for(const width of [390,1280]) {
  test(`Connection security retains sensitive drafts without replay at ${width}px`,async({page},info)=>{
    await page.setViewportSize({width,height:900});
    let auth='sign_in_required', saved=false, mode='bad-token', posts=0;
    const requests:{url:string,body:any}[]=[];
    const oldSha='0123456789abcdef'.repeat(4);
    const newSha='fedcba9876543210'.repeat(4);
    const active={sha256:oldSha,subject:'CN=amux',not_after:1900000000};
    await page.addInitScript(()=>{localStorage.setItem('amux_walkthrough_done','1');});
    await page.route('**/api/connection/security',r=>r.fulfill({json:{auth,can_configure:auth==='owner',tls:{measured:true,n_considered:1,active,saved_sha256:saved?newSha:active.sha256,restart_required:saved,trust:'unknown_to_server'}}}));
    await page.route('**/api/connection/session',async r=>{
      posts++;requests.push({url:r.request().url(),body:r.request().postDataJSON()});
      if(mode==='lost') {await r.abort('failed');return;}
      if(mode==='bad-token') {await r.fulfill({status:401,json:{error:'invalid_owner_token'}});return;}
      auth='owner';await r.fulfill({json:{ok:true,reload:'/api/_clear_sw'},headers:{'set-cookie':'__Host-amux_owner=fixture-session; Path=/; Secure; HttpOnly; SameSite=Lax'}});
    });
    await page.route('**/api/connection/certificate',async r=>{
      posts++;requests.push({url:r.request().url(),body:r.request().postDataJSON()});
      if(!saved) {await r.fulfill({status:422,json:{error:'certificate_invalid_or_key_mismatch'}});return;}
      await r.fulfill({json:{ok:true,saved:{sha256:newSha},applied:false,restart_required:true}});
    });
    await page.route('**/api/_clear_sw',r=>r.fulfill({contentType:'text/html',body:'<p>Existing session bootstrap landing</p>'}));
    await page.goto('/');
    await page.waitForFunction(()=>typeof (window as any)._openConnectionSecurity==='function');
    await page.evaluate(()=>{(window as any)._amuxAuthWithheldBanner();});
    await page.locator('#amux-auth-withheld button').click();
    await expect(page.locator('#connection-security-status')).toContainText('Sign in required');
    await expect(page.locator('#connection-certificate')).toBeDisabled();
    await expect(page.locator('#connection-private-key')).toBeDisabled();
    await expect(page.locator('#connection-certificate-save')).toBeDisabled();
    await page.locator('#connection-owner-token').fill('bad-token-sentinel');
    await page.locator('#connection-sign-in').click();
    await expect(page.locator('#connection-security-error')).toContainText('invalid or has been revoked');
    await expect(page.locator('#connection-owner-token')).toHaveValue('bad-token-sentinel');
    expect(posts).toBe(1);
    expect(await page.evaluate(()=>eval("_outboxQueueable(location.origin+'/api/connection/session',{method:'POST',body:'{}'})"))).toBe(false);
    expect(await page.evaluate(()=>eval("_outboxQueueable(location.origin+'/api/connection/certificate',{method:'POST',body:'{}'})"))).toBe(false);

    // Closing/reopening and read refresh must neither lose the draft nor retry.
    await page.evaluate(()=>{(window as any).closeSettings();});
    await expect(page.locator('#amux-auth-withheld')).toBeVisible();
    await page.locator('#amux-auth-withheld button').click();
    await expect(page.locator('#connection-owner-token')).toHaveValue('bad-token-sentinel');
    await expect(page.locator('#connection-security-error')).toContainText('invalid or has been revoked');
    mode='lost';
    await page.locator('#connection-sign-in').click();
    await expect(page.locator('#connection-security-error')).toContainText('Sign-in did not complete');
    await page.evaluate(async()=>{await (window as any)._connectionSecurityLoad();await (window as any)._connectionSecurityLoad();});
    expect(posts).toBe(2);
    expect(await page.evaluate(()=>JSON.stringify({...localStorage}))).not.toContain('bad-token-sentinel');
    expect(await page.evaluate(()=>eval('_readQueue()').length)).toBe(0);
    // Member status must not expose a certificate mutation control.
    auth='member';await page.getByRole('button',{name:'Refresh security status',exact:true}).click();
    await expect(page.locator('#connection-security-status')).toContainText('Scoped member');
    await expect(page.locator('#connection-certificate')).toBeDisabled();
    await expect(page.locator('#connection-private-key')).toBeDisabled();
    await expect(page.locator('#connection-certificate-save')).toBeDisabled();
    mode='valid';await page.locator('#connection-owner-token').fill('good-token-sentinel');
    await page.locator('#connection-sign-in').click();
    await expect(page).toHaveURL(/\/api\/_clear_sw$/);
    expect(posts).toBe(3);expect(requests[2].body).toEqual({token:'good-token-sentinel'});
    expect(requests.every(r=>!r.url.includes('token'))).toBe(true);
    expect((await page.context().cookies()).find(c=>c.name==='__Host-amux_owner')?.httpOnly).toBe(true);
    await page.goto('/');await page.evaluate(()=>{(window as any)._openConnectionSecurity();});
    await expect(page.locator('#connection-security-status')).toContainText('Signed in as owner');
    await page.locator('#connection-certificate').setInputFiles({name:'server.pem',mimeType:'application/x-pem-file',buffer:Buffer.from('public-cert-sentinel')});
    await page.locator('#connection-private-key').setInputFiles({name:'server-key.pem',mimeType:'application/x-pem-file',buffer:Buffer.from('private-key-sentinel')});
    await page.locator('#connection-certificate-save').click();
    await expect(page.locator('#connection-security-error')).toContainText('key does not match');
    expect(await page.locator('#connection-private-key').evaluate((e:HTMLInputElement)=>e.files?.[0].name)).toBe('server-key.pem');
    expect(posts).toBe(4);
    saved=true;await page.locator('#connection-certificate-save').click();
    await expect(page.locator('#connection-security-error')).toContainText('Restart required');
    await expect(page.locator('#connection-security-status')).toContainText('Loaded fallback certificate SHA-256: '+oldSha);
    await expect(page.locator('#connection-security-status')).toContainText('Saved certificate SHA-256: '+newSha);
    await expect(page.locator('#connection-security-status')).toContainText('Browser/OS trust is not measured');
    expect(posts).toBe(5);expect(await page.evaluate(()=>eval('_readQueue()').length)).toBe(0);
    expect(await page.evaluate(()=>JSON.stringify({...localStorage}))).not.toMatch(/good-token-sentinel|bad-token-sentinel|private-key-sentinel/);
    expect(await page.locator('#connection-private-key').inputValue()).toBe('');
    // Model the explicit restart/read, never auto-restart from the browser.
    active.sha256=newSha;saved=false;
    await page.getByRole('button',{name:'Refresh security status',exact:true}).click();
    await expect(page.locator('#connection-security-status')).toContainText('Loaded fallback certificate SHA-256: '+newSha);
    await expect(page.locator('#connection-security-status')).not.toContainText('saved certificate is not active');
    const geometry=await page.locator('#connection-security').evaluate(section=>{
      const visible=(el:Element)=>{const r=el.getBoundingClientRect();return r.width>0&&r.height>0;};
      const rect=(el:Element)=>{const r=el.getBoundingClientRect();return {id:(el as HTMLElement).id||el.tagName.toLowerCase(),left:r.left,right:r.right,top:r.top,bottom:r.bottom,width:r.width,height:r.height};};
      const direct=Array.from(section.children).filter(visible).map(rect);
      const nested=Array.from(section.querySelectorAll('#connection-certificate-form > *')).filter(visible).map(rect);
      const overlaps:string[]=[];
      for(const list of [direct,nested]) for(let i=0;i<list.length-1;i++) {
        if(list[i].bottom>list[i+1].top+0.5 && list[i].right>list[i+1].left+0.5 && list[i+1].right>list[i].left+0.5) overlaps.push(`${list[i].id}->${list[i+1].id}`);
      }
      const overflowing=Array.from(section.querySelectorAll('#connection-security, #connection-security-status, #connection-security-error, #connection-certificate-form, #connection-certificate, #connection-private-key')).filter(el=>(el as HTMLElement).scrollWidth>(el as HTMLElement).clientWidth+1).map(el=>(el as HTMLElement).id||el.tagName.toLowerCase());
      const sr=section.getBoundingClientRect();
      return {overlaps,overflowing,section:{left:sr.left,right:sr.right,width:sr.width},viewport:innerWidth};
    });
    expect(geometry.overlaps).toEqual([]);
    expect(geometry.overflowing).toEqual([]);
    expect(geometry.section.left).toBeGreaterThanOrEqual(0);
    expect(geometry.section.right).toBeLessThanOrEqual(geometry.viewport);
    await page.screenshot({path:info.outputPath(`connection-security-${width}.png`)});
    expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
    // Existing instance switcher exposes the identical Connect action.
    await page.evaluate(()=>{(window as any).closeSettings();(window as any).openAbout();});
    await page.locator('#server-list .connection-security-summary').click();
    await expect(page.locator('#connection-security')).toBeVisible();
    // Existing connection sync/export boundary admits origins and name only.
    const safe=await page.evaluate(()=>{
      (window as any)._saveConnections([{name:'safe',url:'https://safe.test',token:'secret'},{name:'bad',url:'https://bad.test/?_token=secret'},{name:'bad',url:'https://user:secret@bad.test'}]);
      return (window as any)._loadConnections();
    });
    expect(safe).toEqual([{name:'safe',url:'https://safe.test'}]);
  });
}
