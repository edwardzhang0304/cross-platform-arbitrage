#!/usr/bin/env python3
"""Isolated UI preview. Synthetic accounts only; never proxies to a trading service."""
import argparse,copy,json,math,mimetypes,time,uuid
from http.server import ThreadingHTTPServer,BaseHTTPRequestHandler
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
parser=argparse.ArgumentParser();parser.add_argument('--port',type=int,default=18805);args=parser.parse_args()
extra='p-00000000000000000000000000000002'
def profile(id,market,label,scale):
 legacy=id in ['openai','anth']
 return dict(id=id,market=market,label=label,capital_multiplier=scale,mode='live',control_path=('/' if id=='openai' else '/anth') if legacy else '/accounts/'+id,monitor_path='/'+id+'-inventory' if legacy else '/accounts/'+id+'/monitor',inventory_api='/api/'+id+'-inventory' if legacy else '/api/accounts/'+id+'/inventory',control_api=('/api/portable' if id=='openai' else '/api/anth-portable') if legacy else '/api/accounts/'+id+'/control')
profiles=[profile('openai','openai','OPENAI 账号1',1),profile(extra,'openai','OPENAI 账号2',10),profile('anth','anth','ANTH 账号1',1)]
states={'openai':'paused_entries',extra:'running','anth':None};enabled={'openai':True,extra:True,'anth':False};requests=[]
def payload(p):
 now=int(time.time()*1000);idx=p['id'];scale=p['capital_multiplier'];loaded=states[idx] is not None;groups=19 if idx=='openai' else 9;units=1580 if idx=='openai' else 8430
 c=dict(mode='live',market=p['market'],group_notional=str(15*scale),leverage='3',max_loss_usdc=str(30*scale),grid='5',max_groups=20,book_max_age_ms=1500,account_max_age_ms=3000,accumulation=dict(interval_ms=1800000,max_time_adds=5,quota_scope='grid_stage'))
 snap=dict(created_ms=now-86400000,direction='lighter_short',reason='waiting for executable spread',config=c,status=states[idx],lots=[{}]*groups,closed_groups=28,consecutive_rollbacks=2 if idx=='openai' else 0,entry_margin_block={'rejection':'synthetic_preview'} if idx=='openai' else None,stop_requested=states[idx]=='stopped',positions=[dict(units=-units),dict(units=units)],pending=None,orphan=None)
 accounts=[dict(venue=v,equity=str(e),free_margin=str(f),position_units=u,authenticated=True,observed_ms=now,isolated=True,leverage='3') for v,e,f,u in [('lighter',1120 if scale==10 else 112.40,640 if scale==10 else 28.60,-units),('entropy',1095 if scale==10 else 108.80,625 if scale==10 else 31.20,units)]]
 if idx=='openai':accounts[0]['entry_margin']=dict(isolated_only=True,collateral='0.10',reported_available='28.60')
 return dict(ok=True,data=dict(live_build=True,process_dry_run=False,profile=p,csrf='synthetic-preview-token',view=dict(time_ms=now,snapshot=snap,accounts=accounts,books=[dict(venue=v,connected=True,received_ms=now) for v in ['lighter','entropy']]) if loaded else None,strategy=c,vault_unlocked=True,sleep_prevention=True,data_dir='本地界面预览 · 不读取真实账户',build=dict(version='界面预览',strategy_core_sha256='synthetic000000',dirty=True),notifications=dict(saved=True,unlocked=True,config=dict(app_id='cli_example',receive_id_type='open_id',receive_id='ou_example'),delivery=dict(enabled=enabled[idx],pending=0,error=None) if loaded else None)))
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*a):pass
 def send(self,value,type='application/json; charset=utf-8',status=200):
  raw=json.dumps(value,ensure_ascii=False).encode() if type.startswith('application/json') else value.encode() if isinstance(value,str) else value
  self.send_response(status);self.send_header('Content-Type',type);self.send_header('Cache-Control','no-store');self.end_headers();self.wfile.write(raw)
 def do_GET(self):
  path=self.path.split('?')[0]
  if path=='/preview-requests':return self.send(requests)
  if path=='/api/accounts':return self.send(dict(ok=True,data=dict(csrf='synthetic-catalog-token',profiles=[dict(profile=p,loaded=states[p['id']] is not None,configured=True,status=states[p['id']],held_groups=19 if p['id']=='openai' else 9 if p['id']==extra else 0,notification_enabled=enabled[p['id']]) for p in profiles],shared_market_feeds=[dict(market='openai',consumers=2),dict(market='anth',consumers=1)])))
  for p in profiles:
   if path==p['inventory_api']:
    result=payload(p)
    view=result['data']['view']
    if view:
     now=int(time.time()*1000);base=2000 if p['market']=='anth' else 1588;spread=35 if p['market']=='anth' else 49;total=abs(view['snapshot']['positions'][0]['units']);count=len(view['snapshot']['lots'])
     sizes=[total//count]*(count-1)+[total-(total//count)*(count-1)]
     view['snapshot']['lots']=[dict(id='example-'+str(i),opened_ms=now-(count-i)*3600000,units=u,entry_spread=str(round(spread-12+i*.9,2))) for i,u in enumerate(sizes)]
     view['snapshot']['fills']={str(i):dict(time_ms=now-i*180000,venue='lighter' if i%2==0 else 'entropy',side='sell' if i%2==0 else 'buy',units=sizes[-1],price=str(base if i%2==0 else base-spread),fee='0.002') for i in range(6)}
     view['net_pnl']='12.48';view['profit_accounting']=dict(closed_net_profit='4.26',lots=[dict(lot_id='example-'+str(i),remaining_funding=str(round(.01*i,4)),estimated_exit_net=str(round(-.15+i*.05,4))) for i in range(count)])
     view['books']=[dict(venue=v,connected=True,received_ms=now,bids=[dict(price=str(price),units=50000)],asks=[dict(price=str(price+.2),units=50000)]) for v,price in [('lighter',base),('entropy',base-spread)]]
     view['directional_means']=[str(-spread-.2),str(spread-.2)]
     view['chart_points']=[dict(time_ms=now-(28800-i)*1000,lighter_bid=str(base+math.sin(i/80)*2),lighter_ask=str(base+math.sin(i/80)*2+.2),entropy_bid=str(base-spread+math.cos(i/100)),entropy_ask=str(base-spread+math.cos(i/100)+.2)) for i in range(28801)]
    return self.send(result)
   if path==p['control_path']:
    html=(ROOT/'frontend/portable.html').read_text().replace("fetch('/api/openai-inventory'",f"fetch('{p['inventory_api']}'").replace('/openai-inventory',p['monitor_path']).replace('/api/portable',p['control_api'])
    html=html.replace('/*CONTROL_PROFILE*/','const controlProfile='+json.dumps(p,ensure_ascii=False)+';').replace('/*CONTROL_PROFILES*/','const controlProfiles='+json.dumps(profiles,ensure_ascii=False)+';')
    html=html.replace('START_OPENAI_LIVE_STRATEGY','START_'+p['id'].upper()+'_LIVE_STRATEGY').replace('CONFIRM_OPENAI_ROLLBACK_RECOVERY','CONFIRM_'+p['id'].upper()+'_ROLLBACK_RECOVERY').replace('RESUME_OPENAI_PROTECTED_EXITS','RESUME_'+p['id'].upper()+'_PROTECTED_EXITS')
    html=html.replace('每平台每组约 15U 名义仓位',f"每个平台每组约 {5*p['capital_multiplier']}U 保证金、{15*p['capital_multiplier']}U 持仓金额，亏损保护 {30*p['capital_multiplier']}U")
    if p['market']=='anth':html=html.replace('/10000','/100000')
    # This label is injected by preview server only; never part of production UI.
    html=html.replace('真实账户 · 独立策略与账本','设计预览 · 示例数据 · 无真实交易').replace('class="console-shell"','class="console-shell preview"')
    return self.send(html,'text/html; charset=utf-8')
   if path==p['monitor_path']:
    html=(ROOT/'frontend/openai-live-monitor.html').read_text().replace('/*PROFILE_CONFIG*/','window.InventoryProfile='+json.dumps(dict(id=p['id'],label=p['label'],control_path=p['control_path'],market=p['market'],mode='live',api=p['inventory_api'],quantityDecimals=5 if p['market']=='anth' else 4,symbols=['ANTHROPIC','io:ANTH'] if p['market']=='anth' else ['OPENAI','io:OAI']))+';window.InventoryProfiles='+json.dumps(profiles,ensure_ascii=False)+';')
    html=html.replace('真实账户 · 只读','示例数据 · 只读').replace('OPENAI 实盘监控',p['label']+' 实盘监控')
    return self.send(html,'text/html; charset=utf-8')
  if path=='/accounts':return self.send((ROOT/'frontend/accounts.html').read_text(),'text/html; charset=utf-8')
  if path.startswith('/assets/'):
   name=path[len('/assets/'):];base=ROOT/'frontend';file=base/name if name in ['control-center.css','control-center.js','glass-ui.js','monitor.css','monitor-shell.js','inventory-components.js','openai-market-visuals.js'] else base/'assets'/name
   if file.resolve().is_relative_to(base.resolve()) and file.is_file():return self.send(file.read_bytes(),mimetypes.guess_type(file)[0] or 'application/octet-stream')
  self.send({'error':'not found'},status=404)
 def do_POST(self):
  length=int(self.headers.get('Content-Length','0'));body=json.loads(self.rfile.read(length));requests.append(dict(path=self.path,body={k:v for k,v in body.items() if k in ['command','id','confirmation']}))
  for p in profiles:
   if self.path==p['control_api']:
    command=body.get('command');idx=p['id']
    if command=='stop':states[idx]='stopped'
    if command in ['pause','resume_protected_exits']:states[idx]='paused_entries'
    if command=='start':states[idx]='running'
    if command=='launch':states[idx]='stopped'
    if command=='disable_notifications':enabled[idx]=False
    if command=='save_notifications':enabled[idx]=True
    return self.send(dict(ok=True,data=dict(message='预览操作已完成；没有发送真实指令。',accounts=payload(p)['data']['view']['accounts'] if command=='preflight' and states[idx] else None)))
  self.send(dict(ok=False,error='预览不创建真实账户'))
print(f'Preview: http://127.0.0.1:{args.port}/accounts/{extra}',flush=True)
ThreadingHTTPServer(('127.0.0.1',args.port),Handler).serve_forever()
