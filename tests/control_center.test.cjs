const {test}=require('node:test');
const assert=require('node:assert/strict');
const ui=require('../frontend/control-center.js');
test('display never treats isolated floating profit as funding for a new entry',()=>{
 const a={free_margin:'28.60',entry_margin:{isolated_only:true,collateral:'0.10',reported_available:'28.60'}};
 assert.equal(ui.entryAvailable(a).text,'0.10 U');
 a.entry_margin.collateral=null;assert.equal(ui.entryAvailable(a).text,'待核对');
 a.entry_margin.isolated_only=false;assert.equal(ui.entryAvailable(a).text,'待核对');
 a.entry_margin=null;assert.equal(ui.entryAvailable(a).text,'28.60 U');
 assert.equal(ui.entryAvailable({}).text,'—');
});
test('paired status requires fresh authenticated real evidence and both ledger legs',()=>{
 const v={snapshot:{config:{account_max_age_ms:3000},positions:[{units:-700},{units:700}]},accounts:[{venue:'lighter',position_units:-700,authenticated:true,observed_ms:9000},{venue:'entropy',position_units:700,authenticated:true,observed_ms:9000}]};
 assert.equal(ui.paired(v,10000).ok,true);assert.equal(ui.paired(v,13000).ok,false);
 v.accounts[0].authenticated=false;assert.equal(ui.paired(v,10000).ok,false);v.accounts[0].authenticated=true;
 v.snapshot.pending={};assert.equal(ui.paired(v,10000).ok,false);v.snapshot.pending=null;
 v.snapshot.positions[1].units=0;assert.equal(ui.paired(v,10000).ok,false);v.snapshot.positions[1].units=700;
 v.accounts[1].position_units=null;assert.equal(ui.paired(v,10000).ok,false);v.accounts[1].position_units=700;
 v.snapshot.emergency_exit={flat_confirmed_ms:1};assert.equal(ui.paired(v,10000).ok,false);
});
test('states distinguish protected exits, warmup and flat-but-unreconciled exit',()=>{
 assert.equal(ui.state({status:'paused_entries'}).text,'仅平仓和止损');
 assert.equal(ui.state({status:'warming'}).text,'行情预热中');
 assert.equal(ui.state({status:'running',emergency_exit:{flat_confirmed_ms:1}}).text,'已清仓，账本核对中');
 assert.equal(ui.state(null).text,'未加载');assert.equal(ui.state({status:'needs_attention'}).tone,'bad');
});
test('shared account navigation preserves account scope on both screens and escapes labels',()=>{
 const fs=require('node:fs'),vm=require('node:vm');
 const ctx=vm.createContext({document:{createElement:()=>({setAttribute(k,v){this[k]=v}})}});
 vm.runInContext(fs.readFileSync(require('node:path').join(__dirname,'../frontend/glass-ui.js'),'utf8'),ctx);
 const items=[{profile:{id:'first',label:'OPENAI 账号1',control_path:'/',monitor_path:'/openai-inventory'},loaded:true,status:'running'},{profile:{id:'second',label:'<img onerror=bad()>',control_path:'/accounts/second',monitor_path:'/accounts/second/monitor'},loaded:true,status:'paused_entries'}];
 for(const monitor of [false,true]){
  const element={children:[],replaceChildren(){this.children=[]},append(a){this.children.push(a)},querySelector(){return null}};
  ctx.GlassUI.createAccountNavigation(element,'second',monitor).render(items);
  assert.equal(element.children[0].href,monitor?'/openai-inventory':'/');
  assert.equal(element.children[1].href,monitor?'/accounts/second/monitor':'/accounts/second');
  assert.equal(element.children[1]['aria-current'],'page');
  assert.match(element.children[1].innerHTML,/&lt;img/);
  assert.match(element.children[1].innerHTML,/仅平仓和止损/);
 }
});
test('styled confirmation cancels without a request and submits exactly once only after acceptance',async()=>{
 const fs=require('node:fs'),vm=require('node:vm');const elements=new Map();
 const get=id=>{if(!elements.has(id))elements.set(id,{textContent:'',value:'',open:false,attrs:{},dataset:{},classList:{add(){},remove(){}},setAttribute(k,v){this.attrs[k]=v},addEventListener(){},focus(){this.focused=true},showModal(){this.open=true},close(){this.open=false}});return elements.get(id)};
 const posts=[];const ctx=vm.createContext({document:{getElementById:get,querySelectorAll:()=>[],body:{classList:{add(){},remove(){}}}},MutationObserver:class{observe(){}},setInterval(){},crypto:{randomUUID:()=> 'synthetic-confirm-id'},fetch:async(url,opts)=>{if(opts?.method==='POST')posts.push(JSON.parse(opts.body));return {json:async()=>({ok:true,data:{csrf:'synthetic',profile:{id:'second'},message:'accepted'}})}}});
 vm.runInContext(fs.readFileSync(require('node:path').join(__dirname,'../frontend/glass-ui.js'),'utf8'),ctx);
 vm.runInContext(fs.readFileSync(require('node:path').join(__dirname,'../frontend/control-center.js'),'utf8'),ctx);ctx.ControlCenter.render=()=>{};
 let script=fs.readFileSync(require('node:path').join(__dirname,'../frontend/portable.html'),'utf8').match(/<script>\s*([\s\S]*?)<\/script>/)[1].replace('/*CONTROL_PROFILE*/','const controlProfile={id:"second",label:"OPENAI 账号2"};');
 vm.runInContext(script,ctx);await ctx.refresh();
 const cancelled=ctx.act('close_all','CLOSE_ALL_POSITIONS','最大滑点 5%，允许亏损');
 assert.equal(get('confirmation-dialog').open,true);assert.equal(get('confirmation-cancel').focused,true);
 assert.match(get('confirmation-question').textContent,/OPENAI 账号2.*5%/);assert.equal(posts.length,0);
 ctx.ControlCenter.resolveConfirmation(false);await cancelled;assert.equal(posts.length,0);
 const accepted=ctx.act('close_all','CLOSE_ALL_POSITIONS','最大滑点 5%，允许亏损');
 ctx.ControlCenter.resolveConfirmation(true);ctx.ControlCenter.resolveConfirmation(true);await accepted;
 assert.equal(posts.length,1);assert.equal(posts[0].command,'close_all');assert.equal(posts[0].confirmation,'CLOSE_ALL_POSITIONS');
 assert.equal(get('confirmation-dialog').open,false);
});
