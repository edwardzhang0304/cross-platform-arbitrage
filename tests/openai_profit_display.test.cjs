const {test}=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const vm=require('node:vm');
const path=require('node:path');

function fixture(profile){
  const html=fs.readFileSync(path.join(__dirname,'../frontend/openai-live-monitor.html'),'utf8');
  const code=html.match(/<script>\s*([\s\S]*?)<\/script>/)[1];
  const elements=new Map();
  const element=id=>{if(!elements.has(id))elements.set(id,{});return elements.get(id)};
  const context=vm.createContext({document:{getElementById:element},
    window:{InventoryProfile:profile,OpenaiMarketVisuals:{renderCharts(){},displayDirection(){return 'shortL'},renderQuoteCards(){},quoteRows(){return []}}},
    fetch:()=>new Promise(()=>{}),setInterval(){}});
  vm.runInContext(code,context);
  const payload={ok:true,live_build:true,process_dry_run:false,view:{
    snapshot:{config:{mode:'live'},direction:'lighter_short',lots:[
      {id:'a',opened_ms:1000,units:80,entry_spread:'7.61'},
      {id:'b',opened_ms:2000,units:90,entry_spread:'10.2'}],fills:{}},
    books:[],net_pnl:'-0.28'}};
  return {render:()=>context.render(payload),payload,element};
}

test('old/incomplete accounting shows unknown group profit, not zero or a guessed total',()=>{
  const f=fixture();f.render();
  assert.equal((f.element('lots').innerHTML.match(/<td>—<\/td>/g)||[]).length,4);
  assert.equal(f.element('pnl').textContent,'-0.28 U');
  assert.match(f.element('pnl').title,/已结算资金费/);
  assert.equal(f.element('closed-pnl').textContent,'—');
  f.payload.view.profit_accounting={lots:[{lot_id:'a',remaining_funding:null,estimated_exit_net:null}]};
  f.render();
  assert.equal((f.element('lots').innerHTML.match(/<td>—<\/td>/g)||[]).length,4);
});

test('closed net profit displays independently with signed colors, zero and missing data',()=>{
  const f=fixture();
  for(const [value,text,color] of [
    ['0.47610838','0.48 U','good'],['-0.1234','-0.12 U','badtext'],
    ['0','0.00 U',''],['-0.001','0.00 U',''],[null,'—',''],[undefined,'—',''],['invalid','—','']
  ]){
    f.payload.view.profit_accounting={closed_net_profit:value,lots:[]};
    f.render();
    assert.equal(f.element('closed-pnl').textContent,text);
    assert.equal(f.element('closed-pnl').className,color);
    assert.match(f.element('closed-pnl').title,/不含未平仓浮动盈亏/);
    assert.equal(f.element('pnl').textContent,'-0.28 U');
  }
});

test('group rows bind by ID, use signed funding and never subtract it again from total',()=>{
  const f=fixture();
  f.payload.view.profit_accounting={lots:[
    {lot_id:'b',remaining_funding:'0.012',estimated_exit_net:'0.025'},
    {lot_id:'a',remaining_funding:'-0.031629',estimated_exit_net:'-0.056'}]};
  f.render();
  const rows=f.element('lots').innerHTML.split('</tr>');
  assert.match(rows[0],/class="badtext" title="-0\.031629 U">-0\.03 U/);
  assert.match(rows[0],/class="badtext" title="-0\.056000 U">-0\.06 U/);
  assert.match(rows[1],/class="good" title="0\.012000 U">0\.01 U/);
  assert.equal(f.element('pnl').textContent,'-0.28 U');
  assert.equal(f.element('pnl').className,'badtext');
});


test('ANTH paper page checks market/mode and keeps five decimal quantities',()=>{
  const f=fixture({market:'anth',mode:'paper',quantityDecimals:5,api:'/api/anth-inventory',symbols:['ANTHROPIC','io:ANTH']});
  assert.throws(()=>f.render(),/不匹配/);
  Object.assign(f.payload,{live_build:false,paper_build:true,process_dry_run:true});
  f.payload.view.snapshot.config={market:'anth',mode:'paper'};
  f.payload.view.snapshot.lots[0].units=637;
  f.render();assert.match(f.element('lots').innerHTML,/0\.00637/);
  assert.equal(f.element('mode-tag').textContent,'模拟账户 · 虚拟资金');
  f.payload.view.snapshot.config.market='openai';assert.throws(()=>f.render(),/不匹配/);
  f.payload.view.snapshot.config.market='anth';f.payload.view.snapshot.config.mode='live';assert.throws(()=>f.render(),/不匹配/);
});
