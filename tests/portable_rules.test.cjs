const {test}=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const vm=require('node:vm');
const path=require('node:path');

test('parameter text describes the loaded rules, including unchanged legacy accounts',()=>{
  const html=fs.readFileSync(path.join(__dirname,'../frontend/portable.html'),'utf8');
  const script=html.match(/<script>\s*([\s\S]*?)<\/script>/)[1];
  const ctx=vm.createContext({document:{getElementById(){return {}}},
    fetch:()=>new Promise(()=>{}),setInterval(){}});
  vm.runInContext(script,ctx);
  const newer=JSON.parse(fs.readFileSync(path.join(__dirname,'../config/strategy.example.json'),'utf8'));
  const legacy=JSON.parse(fs.readFileSync(path.join(__dirname,'fixtures/inventory/live-strategy.json'),'utf8'));
  assert.match(ctx.entryRulesText(newer),/网格间隔 5U；同价加仓至少 60 分钟，每个网格阶段最多 5 次/);
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
