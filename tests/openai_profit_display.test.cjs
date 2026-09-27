const {test}=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const vm=require('node:vm');
const path=require('node:path');

function fixture(){
  const html=fs.readFileSync(path.join(__dirname,'../frontend/openai-live-monitor.html'),'utf8');
  const code=html.match(/<script>\s*([\s\S]*?)<\/script>/)[1];
  const elements=new Map();
  const element=id=>{if(!elements.has(id))elements.set(id,{});return elements.get(id)};
  const context=vm.createContext({document:{getElementById:element},
    window:{OpenaiMarketVisuals:{renderCharts(){},displayDirection(){return 'shortL'},renderQuoteCards(){},quoteRows(){return []}}},
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
  f.payload.view.profit_accounting={lots:[{lot_id:'a',remaining_funding:null,estimated_exit_net:null}]};
  f.render();
  assert.equal((f.element('lots').innerHTML.match(/<td>—<\/td>/g)||[]).length,4);
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
