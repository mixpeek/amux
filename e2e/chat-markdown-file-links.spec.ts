import {test,expect} from './fixtures';
import {mkdtemp,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';

declare const _chat:any;
declare let peekSessionDir:string;

const end='CHAT_MARKDOWN_FILE_FINAL_LINE_20261010';
test('Markdown file anchors open complete real files in finished, interrupted and incremental Chat replies',async({page},info)=>{
 const beacons:any[]=[];page.on('request',r=>{if(r.url().endsWith('/api/client-debug')&&r.method()==='POST'){try{const x=r.postDataJSON();if(x.action==='chat-markdown-file-open')beacons.push(x);}catch{}}});
 const dir=await mkdtemp(join(tmpdir(),'amux-chat-links-'));
 const md=join(dir,'proof file.md'),txt=join(dir,'proof.txt');
 await writeFile(md,Array.from({length:150},(_,i)=>`${i+1}. Complete artifact evidence`).join('\n\n')+'\n\n'+end);
 await writeFile(txt,'plain text\n'+end);
 await page.route(/\/api\/sessions(?:\?.*)?$/,r=>r.fulfill({json:[{name:'chat-file-fixture',provider:'codex',chat_companion:true,running:false,dir}]}));
 try{
  await page.goto('/?peekEmbed=chat-file-fixture&peekTab=chat');
  await expect(page.locator('#peek-overlay')).toBeVisible();
  await expect.poll(()=>page.evaluate(()=>typeof (window as any).renderMarkdown)).toBe('function');
  const render=async(kind:string,text:string)=>page.evaluate(({kind,text,dir})=>{
   const w=window as any;w._chatUnmount();peekSessionDir=dir;_chat.name='chat-file-fixture@chat';_chat.messages=[];_chat.streaming=null;
   const body=document.getElementById('peek-body')!;body.classList.add('peek-chat');
   if(kind==='finished')_chat.messages=[{role:'assistant',text,ts:Date.now()/1000}];
   if(kind==='interrupted')_chat.messages=[{role:'assistant',text,interrupted:true,ts:Date.now()/1000}];
   if(kind==='streaming'){_chat.streaming=w._chatNewTurn({turn_id:'file-link-stream'});w._chatApply(_chat.streaming,{type:'delta',text});}
   w._chatRender(); if(kind==='streaming'){w._chatApply(_chat.streaming,{type:'delta',text:'\n\nIncremental tail'});w._chatPaintLive();}
  },{kind,text,dir});
  const replies=[['finished',`[absolute artifact](<${md}>)`],['finished','[relative artifact](<./proof file.md>)'],['interrupted',`[interrupted artifact](<${txt}>)`],['streaming',`[streamed artifact](<${md}>)`]];
  for(const[kind,text]of replies){
   await render(kind,text);
   const link=page.locator('.chat-bubble a.md-file-link');await expect(link).toHaveCount(1);
   const before=page.url();await link.click();await expect(page.locator('#file-overlay')).toBeVisible();await expect(page.locator('#file-body')).toContainText(end);expect(page.url()).toBe(before);
   await expect.poll(()=>page.locator('#file-body').evaluate((el,needle)=>{
    el.scrollTop=el.scrollHeight;const node=[...el.querySelectorAll('p')].find(x=>x.textContent?.includes(needle));
    if(!node)return el.textContent?.includes(needle)&&el.scrollHeight-el.clientHeight-el.scrollTop<2;
    const r=node.getBoundingClientRect(),b=el.getBoundingClientRect();return r.top>=b.top-1&&r.bottom<=Math.min(b.bottom,innerHeight)+1&&el.contains(document.elementFromPoint(r.left+10,r.bottom-2));
   },end)).toBe(true);
   await page.screenshot({path:info.outputPath(kind+'-'+replies.indexOf(replies.find(x=>x[1]===text)!)+'-file-bottom.png')});
   await page.locator('#file-overlay button[onclick="closeFilePreview()"]').click();
  }
  await expect.poll(()=>beacons.length).toBe(4);expect(beacons.every(x=>x.measured===true&&x.n===1)).toBe(true);
  await render('finished','[external](https://example.com/docs/proof.md) [heading](#evidence) [bad](javascript:alert(1))');
  await expect(page.locator('.chat-bubble a.md-file-link')).toHaveCount(0);await expect(page.locator('.chat-bubble a[href="https://example.com/docs/proof.md"]')).toHaveAttribute('target','_blank');await expect(page.locator('.chat-bubble a[href^="javascript:"]')).toHaveCount(0);
 }finally{await rm(dir,{recursive:true,force:true});}
});
