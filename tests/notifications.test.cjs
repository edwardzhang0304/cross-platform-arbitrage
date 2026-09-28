const {test}=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const vm=require('node:vm');
const path=require('node:path');

function page(fetchPost){
  const html=fs.readFileSync(path.join(__dirname,'../frontend/portable.html'),'utf8');
  const elements=new Map();
  const get=id=>{if(!elements.has(id))elements.set(id,{value:'',textContent:''});return elements.get(id)};
  const buttons=[{dataset:{defensive:'true'},disabled:false},{dataset:{},disabled:false}];
  const ctx=vm.createContext({document:{getElementById:get,querySelectorAll:()=>buttons},
    crypto:{randomUUID:()=> 'synthetic-command-id'},setInterval(){},confirm:()=>true,
    fetch:async(url,opts)=>opts?.method==='POST'?fetchPost(JSON.parse(opts.body)):
      {json:async()=>({ok:true,data:{csrf:'synthetic-token',notifications:{saved:false,unlocked:false}}})}});
  vm.runInContext(html.match(/<script>\s*([\s\S]*?)<\/script>/)[1],ctx);
  return {ctx,get,buttons};
}

test('a hung notification request leaves defensive trading controls available',async()=>{
  const sent=[];
  const {ctx,buttons}=page(async request=>{sent.push(request.command);if(request.command==='test_notifications')return new Promise(()=>{});return {json:async()=>({ok:true,data:{accepted:true}})}});
  await ctx.refresh();
  void ctx.act('test_notifications');
  assert.equal(buttons[0].disabled,false);
  assert.equal(buttons[1].disabled,true);
  await ctx.act('stop');
  assert.deepEqual(sent,['test_notifications','stop']);
  assert.equal(buttons[0].disabled,false);
  assert.equal(buttons[1].disabled,true);
});

test('saving notifications clears secret inputs and status reports disabled or failed delivery',async()=>{
  let saved;
  const {ctx,get}=page(async request=>{saved=request;return {json:async()=>({ok:true,data:{accepted:true}})}});
  await ctx.refresh();
  for(const [k,v] of Object.entries({feishu_app_id:'cli_synthetic',feishu_app_secret:'synthetic-secret',feishu_receive_id_type:'open_id',feishu_receive_id:'ou_synthetic'}))get(k).value=v;
  await ctx.saveNotifications();
  assert.equal(saved.command,'save_notifications');assert.equal(saved.feishu_app_secret,'synthetic-secret');
  assert.equal(get('feishu_app_secret').value,'');
  assert.equal(saved.feishu_receive_id_type,'open_id');
  assert.equal(saved.feishu_receive_id,'ou_synthetic');
  ctx.renderNotificationStatus({saved:true,unlocked:true,config:{app_id:'cli_example',receive_id_type:'open_id',receive_id:'ou_example'},delivery:{enabled:true,pending:3,dropped:2,error:'发送失败，将重试'}});
  assert.match(get('notification-status').textContent,/待发 3 条/);
  assert.match(get('notification-status').textContent,/已丢弃最旧 2 条/);
  assert.match(get('notification-status').textContent,/发送失败/);
  assert.equal(get('feishu_app_secret').value,'');
});
