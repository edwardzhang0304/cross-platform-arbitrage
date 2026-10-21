const {test}=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const vm=require('node:vm');
const path=require('node:path');

test('stop and close receipts remain distinct from completed execution, even during slow commands',async()=>{
  const html=fs.readFileSync(path.join(__dirname,'../frontend/portable.html'),'utf8');
  const elements=new Map();const element=id=>{if(!elements.has(id))elements.set(id,{});return elements.get(id)};
  const posts=[];let polls=0,tick;
  const payload={ok:true,data:{csrf:'synthetic',view:{snapshot:{status:'needs_attention',lots:[],closed_groups:0,
    paused:true,stop_requested:true,close_requested:true,pending:{action:'close'}}}}};
  const ctx=vm.createContext({document:{getElementById:element,querySelectorAll:()=>[]},
    crypto:{randomUUID:()=>`synthetic-${posts.length}`},confirm:()=>true,
    fetch:(url,opts)=>{if(opts?.method==='POST')return new Promise(resolve=>posts.push({body:JSON.parse(opts.body),resolve}));
      polls++;return Promise.resolve({json:async()=>payload});},setInterval:fn=>{tick=fn;}});
  vm.runInContext(html.match(/<script>\s*([\s\S]*?)<\/script>/)[1],ctx);
  await ctx.refresh();
  const stop=ctx.act('stop');
  assert.match(element('notice').textContent,/等待后台确认/);
  await ctx.act('stop');assert.equal(posts.length,1,'same pending command is not sent twice');
  const before=polls;await tick();assert.ok(polls>before,'status polls continue while a command waits');
  await ctx.refresh();assert.match(element('control-status').textContent,/已记录全部平仓请求.*未完成订单/);
  posts[0].resolve({json:async()=>({ok:true,data:{accepted:true}})});await stop;
  assert.match(element('notice').textContent,/停止新交易请求已记录/);
  const close=ctx.act('close_all');assert.equal(posts.length,2);
  posts[1].resolve({json:async()=>({ok:true,data:{accepted:true}})});await close;
  assert.match(element('notice').textContent,/不代表已全部成交/);
  assert.match(ctx.controlStateText({status:'stopped',stop_requested:true,pending:null}),/已有持仓不会/);
  assert.match(ctx.controlStateText({status:'needs_attention',close_requested:true,pending:null}),/异常尚需复核/);
});

test('parameter text describes the loaded rules, including unchanged legacy accounts',()=>{
  const html=fs.readFileSync(path.join(__dirname,'../frontend/portable.html'),'utf8');
  const script=html.match(/<script>\s*([\s\S]*?)<\/script>/)[1];
  const ctx=vm.createContext({document:{getElementById(){return {}}},
    fetch:()=>new Promise(()=>{}),setInterval(){}});
  vm.runInContext(script,ctx);
  const newer=JSON.parse(fs.readFileSync(path.join(__dirname,'../config/strategy.example.json'),'utf8'));
  const legacy=JSON.parse(fs.readFileSync(path.join(__dirname,'fixtures/inventory/live-strategy.json'),'utf8'));
  assert.match(ctx.entryRulesText(newer),/网格间隔 5U；同价加仓至少 30 分钟，每个网格阶段最多 5 次/);
  assert.match(ctx.entryRulesText(newer),/双腿成交配平后重置/);
  assert.match(ctx.entryRulesText(legacy),/网格间隔 2U；同价加仓至少 15 分钟，整轮最多 5 次/);
  assert.equal(ctx.entryRulesText(null),'请先配置或导入账户');
  assert.match(ctx.entryRulesText({...newer,group_notional:'150',max_loss_usdc:'300'}),/每组每边保证金约 50U（持仓金额约 150U），逐仓 3 倍；亏损保护 300U/);
});

test('manual rollback acknowledgement is explicit, market-bound and never starts trading',async()=>{
  const html=fs.readFileSync(path.join(__dirname,'../frontend/portable.html'),'utf8');
  const elements=new Map();const element=id=>{if(!elements.has(id))elements.set(id,{});return elements.get(id)};
  const posts=[];let accept=false;
  const snapshot={status:'stopped',stop_requested:true,paused:true,consecutive_rollbacks:3,lots:[],closed_groups:0};
  const ctx=vm.createContext({document:{getElementById:element,querySelectorAll:()=>[]},
    crypto:{randomUUID:()=>`manual-${posts.length}`},confirm:()=>accept,setInterval(){},
    fetch:async(url,opts)=>opts?.method==='POST'?(posts.push({url,body:JSON.parse(opts.body)}),{json:async()=>({ok:true,data:{message:'verified; still stopped'}})}):
      {json:async()=>({ok:true,data:{csrf:'synthetic',view:{snapshot}}})}});
  vm.runInContext(html.match(/<script>\s*([\s\S]*?)<\/script>/)[1],ctx);
  await ctx.refresh();assert.equal(element('rollback-recovery').hidden,false);
  const call=html.match(/onclick="(act\('confirm_rollback_recovery'[^\"]+)"/)[1];
  await vm.runInContext(call,ctx);assert.equal(posts.length,0,'cancelled confirmation sends nothing');
  accept=true;await vm.runInContext(call,ctx);
  assert.equal(posts.length,1);assert.equal(posts[0].body.command,'confirm_rollback_recovery');
  assert.equal(posts[0].body.confirmation,'CONFIRM_OPENAI_ROLLBACK_RECOVERY');
  assert.match(element('notice').textContent,/still stopped/);
  snapshot.consecutive_rollbacks=0;await ctx.refresh();assert.equal(element('rollback-recovery').hidden,true);
  snapshot.entry_margin_block={rejection:'margin_not_allowed'};await ctx.refresh();
  assert.equal(element('rollback-recovery').hidden,false);assert.match(element('control-status').textContent,/保证金拒单/);
  assert.equal(posts.length,1,'success must not send start or any order');
});

test('paper console renders both core identities without shadowing response data',async()=>{
  const html=fs.readFileSync(path.join(__dirname,'../frontend/paper.html'),'utf8');
  const elements=new Map();
  const element=()=>({children:[],append(x){this.children.push(x);if(x.id)elements.set(x.id,x)},replaceChildren(){this.children=[]}});
  elements.set('profiles',element());
  const config=JSON.parse(fs.readFileSync(path.join(__dirname,'../config/strategy.example.json'),'utf8'));
  const payload={ok:true,data:{paper_build:true,live_build:false,csrf:'synthetic',
    build:{version:'test',strategy_core_sha256:'a'.repeat(64)},rules_fingerprint:'b'.repeat(64),
    view:{snapshot:{config,status:'stopped',lots:[],closed_groups:0,time_adds_used:0}}}};
  const ctx=vm.createContext({document:{getElementById:id=>elements.get(id),createElement:element},
    fetch:async()=>({json:async()=>payload}),setInterval(){}});
  vm.runInContext(html.match(/<script>\s*([\s\S]*?)<\/script>/)[1],ctx);
  await ctx.refresh();
  for(const market of ['openai','anth']){
    const text=elements.get(market).children.map(e=>e.textContent||'').join('\n');
    assert.match(text,/核心 a{12} · 参数 b{12}/);
    assert.match(text,/每个网格阶段最多 5 次/);
  }
});

test('emergency close has explicit 5 percent confirmation and physical progress, no cancel button',()=>{
 const html=fs.readFileSync(path.join(__dirname,'../frontend/portable.html'),'utf8');
 const paper=fs.readFileSync(path.join(__dirname,'../frontend/paper.html'),'utf8');
 assert.doesNotMatch(html,/cancel_close_all|取消全部平仓请求/);assert.doesNotMatch(paper,/cancel_close_all/);
 assert.match(html,/最大滑点 5%.*允许亏损/);
 const ctx=vm.createContext({document:{getElementById(){return {}}},fetch:()=>new Promise(()=>{}),setInterval(){}});
 vm.runInContext(html.match(/<script>\s*([\s\S]*?)<\/script>/)[1],ctx);
 const e={remaining_units:[-700,0],warnings:[],observed_ms:[1,1]};
 assert.match(ctx.controlStateText({config:{market:'anth'},emergency_exit:e}),/剩余 Lighter 0\.007 \/ Entropy 0/);
 assert.match(ctx.controlStateText({emergency_exit:{...e,remaining_units:[0,0],flat_confirmed_ms:1}}),/原订单与成交记录仍在核对/);
 assert.equal(ctx.strategyStateText({status:'recovering',emergency_exit:{...e,remaining_units:[0,0],flat_confirmed_ms:1}}),'已清仓，正在核对账本');
 assert.equal(ctx.strategyStateText({status:'closing',emergency_exit:e}),'正在平仓');
 assert.match(ctx.controlStateText({status:'stopped',emergency_exit:{...e,completed_ms:1}}),/紧急全部平仓完成/);
});

test('same-market control page rejects a foreign profile payload before accepting its token',async()=>{
  let script=fs.readFileSync(path.join(__dirname,'../frontend/portable.html'),'utf8').match(/<script>\s*([\s\S]*?)<\/script>/)[1];
  script=script.replace('/*CONTROL_PROFILE*/','const controlProfile={id:"second",label:"OPENAI second"};');
  const elements=new Map();const element=id=>{if(!elements.has(id))elements.set(id,{});return elements.get(id)};
  const ctx=vm.createContext({document:{getElementById:element},setInterval(){},fetch:async()=>({json:async()=>({ok:true,data:{profile:{id:'openai',market:'openai'},csrf:'foreign-token'}})})});
  vm.runInContext(script,ctx);await ctx.refresh();assert.equal(vm.runInContext('token',ctx),'');
});

test('account management renders same-market accounts separately and retries creation with the same ID',async()=>{
  const script=fs.readFileSync(path.join(__dirname,'../frontend/accounts.html'),'utf8').match(/<script>\s*([\s\S]*?)<\/script>/)[1];
  const element=()=>({children:[],append(...xs){this.children.push(...xs)},replaceChildren(...xs){this.children=xs}});
  const elements=new Map(['accounts','notice','add','market','label','feed','capital'].map(k=>[k,element()]));
  elements.get('market').value='openai';elements.get('label').value='OPENAI <second>';elements.get('capital').value='10';
  const profiles=['openai','second','anth'].map(id=>({profile:{id,label:id==='second'?'OPENAI <second>':id,market:id==='anth'?'anth':'openai',control_path:`/accounts/${id}`,monitor_path:`/accounts/${id}/monitor`},loaded:true,status:'running',held_groups:id==='second'?9:2}));
  const posts=[];let fail=true;let uuid=0;const location={};
  const ctx=vm.createContext({document:{getElementById:id=>elements.get(id),createElement:element},location,setInterval(){},crypto:{randomUUID:()=>`request-${++uuid}`},fetch:async(url,opts)=>{
    if(opts?.method==='POST'){posts.push(JSON.parse(opts.body));if(fail)throw Error('response lost');return {json:async()=>({ok:true,data:{profile:{control_path:'/accounts/second'}}})};}
    return {json:async()=>({ok:true,data:{csrf:'management-token',profiles,shared_market_feeds:[{market:'openai',consumers:2}]}})};
  }});
  vm.runInContext(script,ctx);await ctx.refresh();assert.equal(elements.get('accounts').children.length,3);
  assert.equal(elements.get('accounts').children[1].children[0].textContent,'OPENAI <second>');
  assert.match(elements.get('feed').textContent,/2 套账户/);
  await ctx.addAccount();fail=false;await ctx.addAccount();assert.equal(posts.length,2);assert.equal(posts[0].id,posts[1].id);
  assert.equal(posts[0].capital_multiplier,10);assert.equal(posts[0].confirmation,'CREATE_ACCOUNT_PROFILE');assert.equal(location.href,'/accounts/second');
  assert.equal(elements.get('add').disabled,false);assert.ok(posts.every(p=>!p.command),'creating a slot never loads or starts trading');
});


test('an unloaded account retains its safe bootstrap cause across refresh without starting trading',async()=>{
  const html=fs.readFileSync(path.join(__dirname,'../frontend/portable.html'),'utf8');
  const elements=new Map();const element=id=>{if(!elements.has(id))elements.set(id,{});return elements.get(id)};
  let failure={at_ms:100,operation:'launch',issue:{stage:'entropy_contract',code:'margin_mode_unsupported',message:'Entropy ANTH 保证金模式不受支持'}};
  const ctx=vm.createContext({document:{getElementById:element,querySelectorAll:()=>[]},setInterval(){},
    fetch:async(url,opts)=>{assert.ok(!opts?.method,'status refresh never posts a trading control');return {json:async()=>({ok:true,data:{view:null,startup_failure:failure}})}}});
  vm.runInContext(html.match(/<script>\s*([\s\S]*?)<\/script>/)[1],ctx);
  await ctx.refresh();assert.match(element('summary').textContent,/未加载/);
  assert.match(element('control-status').textContent,/entropy_contract.*margin_mode_unsupported/);
  failure=null;await ctx.refresh();assert.equal(element('control-status').textContent,'');
});

test('protected exits button preserves the latch and distinguishes stopped, active and faulty states',async()=>{
  const html=fs.readFileSync(path.join(__dirname,'../frontend/portable.html'),'utf8');
  const elements=new Map();const element=id=>{if(!elements.has(id))elements.set(id,{});return elements.get(id)};
  const snapshot={status:'stopped',paused:true,stop_requested:true,entry_margin_block:{rejection:'preflight_funding'},lots:[{}],closed_groups:0};
  const posts=[];let accept=false;
  const ctx=vm.createContext({document:{getElementById:element,querySelectorAll:()=>[]},
    crypto:{randomUUID:()=>`exit-${posts.length}`},confirm:()=>accept,setInterval(){},
    fetch:async(url,opts)=>{
      if(opts?.method==='POST'){
        posts.push(JSON.parse(opts.body));snapshot.status='paused_entries';snapshot.stop_requested=false;
        return {json:async()=>({ok:true,data:{message:'仅平仓和止损已启用；保证金保护保留'}})};
      }
      return {json:async()=>({ok:true,data:{csrf:'synthetic',view:{snapshot}}})};
    }});
  vm.runInContext(html.match(/<script>\s*([\s\S]*?)<\/script>/)[1],ctx);
  await ctx.refresh();assert.equal(element('protected-exits').hidden,false);
  assert.match(element('control-status').textContent,/已停止，普通自动平仓未启用/);
  assert.match(element('rollback-count').textContent,/普通自动平仓未启用/);
  const call=html.match(/onclick="(act\('resume_protected_exits'[^\"]+)"/)[1];
  await vm.runInContext(call,ctx);assert.equal(posts.length,0);
  accept=true;await vm.runInContext(call,ctx);
  assert.equal(posts.length,1);assert.equal(posts[0].command,'resume_protected_exits');
  assert.equal(posts[0].confirmation,'RESUME_OPENAI_PROTECTED_EXITS');
  assert.match(element('summary').textContent,/仅平仓和止损/);
  assert.match(element('control-status').textContent,/保证金拒单保护保留，禁止开仓和加仓/);
  assert.ok(snapshot.entry_margin_block);assert.equal(snapshot.paused,true);
  snapshot.status='needs_attention';await ctx.refresh();
  assert.doesNotMatch(element('rollback-count').textContent,/仅平仓和止损已启用/);
  snapshot.status='stopped';snapshot.stop_requested=true;await ctx.refresh();
  assert.match(element('control-status').textContent,/普通自动平仓未启用/);
  delete snapshot.entry_margin_block;await ctx.refresh();assert.equal(element('protected-exits').hidden,true);
  assert.equal(posts.length,1,'polling and mode changes must not submit additional controls');
});
