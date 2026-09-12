import assert from 'node:assert/strict';

// Fault injection around the shipped dashboard, with real Safari/XCTest input.
// This is display/recovery coverage; the Rust test covers the real SQL join.
export function messageScenarios() {
  const name='ios-fixture';
  const message='Review mobile scrolling and preserve this complete message.';
  let reads=0,posts=0,accepted=false,hits=0;
  const active=mode=>mode.startsWith('message-');
  function seed(mode,runId) {
    if(!active(mode))return '';
    if(mode==='message-header')return "localStorage.removeItem('amux_offline_queue');";
    const steering=mode==='message-steering';
    const queue=[{id:'ios-pending',url:'/api/sessions/'+name+(steering?'/steer':'/send'),
      options:{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({text:message,msg_id:'ios-transport'})},
      timestamp:Date.now()-1000,attempted_at:Date.now()-500,state:'blocked',error:'409: previous message acceptance is uncertain'}];
    const history=[{session:name,text:message,type:'direct',time:queue[0].timestamp,msg_id:'ios-transport'}];
    const key='ios-messages-'+runId+'-'+mode;
    return `if(!sessionStorage.getItem(${JSON.stringify(key)})){sessionStorage.setItem(${JSON.stringify(key)},'1');localStorage.setItem('amux_offline_queue',${JSON.stringify(JSON.stringify(queue))});localStorage.setItem('amux_cmd_history',${JSON.stringify(JSON.stringify(history))});}`;
  }
  function route(mode,req) {
    if(!active(mode))return null;
    const url=new URL(req.url,'http://fixture');
    const reply=(body,status=200)=>{hits++;return {body,status};};
    if(url.pathname==='/api/sessions' && mode==='message-header')return reply(Array.from({length:52},(_,i)=>({name:'header-'+i,running:true,status:'working',dir:'/workspace',provider:'codex',rate_limited_until:i<18?Date.now()/1000+3600:null})));
    if(url.pathname==='/api/sessions')return reply([{name,running:true,status:'working',provider:'codex',model:'test',dir:'/workspace',
      steering:mode==='message-steering'?[{id:'server-one',transport_id:'ios-transport',text:message,queued_at:Date.now()/1000},
        {id:'server-two',transport_id:'different-send',text:message,queued_at:Date.now()/1000}]:[]}]);
    if(url.pathname==='/api/sessions/'+name+'/send'){
      if(req.method==='POST'){posts++;return reply({error:'Unexpected command replay in receipt-only test'},500);}
      reads++;
      const id=url.searchParams.get('msg_id');
      assert.equal(id,mode==='message-steering'?'steer:ios-transport':'ios-transport');
      return reply(accepted?{accepted:true,msg_id:id,id:'server-one'}:{accepted:false,msg_id:id},accepted?200:202);
    }
    if(url.pathname==='/api/history')return reply(accepted?[{id:1,session:name,text:message,type:'direct',time:Date.now(),msg_id:'ios-transport'}]:[]);
    if(url.pathname==='/api/sessions/'+name+'/peek')return reply({name,live:'Simulator fixture: no production worker is being driven.',history:''});
    if(url.pathname==='/api/sessions/'+name+'/subagents')return reply({session:name,subagents:[]});
    if(url.pathname==='/api/sessions/'+name+'/steer')return reply([]);
    return null;
  }
  async function run({setMode,navigate,until,evaluate,api,shot,pass,runId}) {
    for(const mode of ['message-duplicates','message-steering','message-retry']){
      reads=0;posts=0;hits=0;accepted=false;setMode(mode);
      const tab=mode==='message-steering'?'steering':'messages';
      const suffix='/?iosCase='+mode+'-'+runId+'#peek='+name+'&tab='+tab;
      await navigate(suffix);
      await until('document.querySelector("#peek-overlay").classList.contains("active")',v=>v);
      await until('document.querySelector("#peek-tab-'+tab+'").classList.contains("active")',v=>v);
      const selector=mode==='message-steering'?'#peek-steering-list':'#peek-messages-list';
      await until(`document.querySelector(${JSON.stringify(selector)}).innerText.includes(${JSON.stringify(message)})`,v=>v);
      await until('JSON.parse(localStorage.getItem("amux_offline_queue"))[0]?.attempts',v=>v>=1);
      assert(reads>0 && hits>0,'The fixture must actually receive safe receipt reads');
      assert.equal(posts,0);
      const visibleText=await evaluate(`document.querySelector(${JSON.stringify(selector)}).innerText`);
      assert.equal(visibleText.split(message).length-1,mode==='message-steering'?2:1,'One visible row per send; intentional repeated text must survive');
      assert.equal(await evaluate('JSON.parse(localStorage.getItem("amux_offline_queue")).length'),1,'A display join must retain durable delivery tracking');
      const bounds=await evaluate(`(()=>{const e=document.querySelector('#peek-tab-${tab}'),r=e.getBoundingClientRect();return {left:r.left,right:r.right,width:innerWidth}})()`);
      assert(bounds.left>=0 && bounds.right<=bounds.width-40,'Selected tab remains visible beside its content');
      if(mode!=='message-steering')assert((await evaluate('document.querySelector("#peek-messages-list").firstElementChild.getBoundingClientRect().height'))<145,'Template whitespace must not inflate pending rows');
      await shot('simulator-'+mode);
      if(mode==='message-retry'){
        const prior=reads;accepted=true;
        await navigate(suffix);
        await until('JSON.parse(localStorage.getItem("amux_offline_queue")||"[]").length',v=>v===0,25000);
        assert(reads>prior,'Reload must resume receipt checks without a manual Retry');assert.equal(posts,0);
        await shot('simulator-message-retry-recovered');
        pass('native legacy uncertain-send reload recovery with zero command POSTs');
      }else pass('native '+(mode==='message-steering'?'steering identity join with intentional repeat preserved':'Messages identity join without duplicate local history'));
    }
    setMode('message-header');
    await navigate('/?iosCase=header-'+runId+'#view=sessions');
    await until('document.querySelector("#active-count").textContent',v=>v==='52');
    await until('!document.querySelector("#peek-overlay").classList.contains("active")',v=>v);
    assert.deepEqual(await evaluate('_headerLayoutCheck()'),[]);
    const header=await evaluate(`(()=>{const ids=['brand-header','conn-status','notif-btn','rate-limit-pill','active-btn','add-btn','settings-btn','interaction-feedback'];return {brand:getComputedStyle(document.querySelector('#brand-name-header'),'::after').content,connection:getComputedStyle(document.querySelector('#conn-status')).fontSize,limited:document.querySelector('#rate-limit-pill-count').textContent,height:document.querySelector('.header-row').getBoundingClientRect().height,controls:ids.map(id=>{const e=id==='interaction-feedback'?document.querySelector('#interaction-feedback > summary'):document.getElementById(id);const r=e.getBoundingClientRect();const hit=document.elementFromPoint(r.x+r.width/2,r.y+r.height/2);return {id,width:r.width,height:r.height,visible:r.left>=0&&r.right<=innerWidth,clickable:e===hit||e.contains(hit)}})}})()`);
    assert.equal(header.brand,'"a"');assert.equal(header.connection,'0px');assert.equal(header.limited,'18');assert(header.height<=64);
    for(const control of header.controls){assert(control.width>=44&&control.height>=44,JSON.stringify(control));assert(control.visible&&control.clickable,JSON.stringify(control));}
    await shot('simulator-compact-header');
    await api('action',{action:'click',selector:'#settings-btn'});
    await until('document.querySelector("#settings-menu").classList.contains("open")',v=>v);
    await shot('simulator-loaded-header-settings');
    await api('action',{action:'click',selector:'#settings-btn'});
    await api('action',{action:'click',selector:'#add-btn'});
    await until('document.querySelector("#add-menu").classList.contains("open")',v=>v);
    await shot('simulator-loaded-header-add');
    await api('action',{action:'click',selector:'#add-btn'});
    pass('native loaded-fleet header Settings and Add menus remain visible and operable');
  }
  return {active,seed,route,run};
}
