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
