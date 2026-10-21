"""Synthetic private-shaped inputs; never reads accounts or user data."""
import json
import pathlib
import sqlite3
import sys
import time

root = pathlib.Path(sys.argv[1])
extra_id = 'p-'+'1'*32
profiles = [dict(id='openai',market='openai',label='OPENAI first'), dict(id='anth',market='anth',label='ANTH first'), dict(id=extra_id,market='openai',label='OPENAI second',capital_multiplier=10)]
def ledger_path(profile):
    return root/('runtime/openai-inventory/live.sqlite' if profile['id']=='openai' else f"profiles/{profile['id']}-live/runtime/inventory.sqlite")
if len(sys.argv)>2 and sys.argv[2]=='--serve':
    from http.server import BaseHTTPRequestHandler, HTTPServer
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass
        def do_GET(self):
            if self.path=='/health':
                value=dict(application='openai-paired-trader',version='synthetic',data_dir=str(root),build=dict(source_commit='synthetic'),profiles=profiles,shared_market_feeds=[dict(market='openai',consumers=2,public_connections=2,received_ms=[1,1])],entropy_info=dict(available=True,limit=900,routine_limit=300,used_weight=432,routine_weight=300,local_deferrals=7,server_cooldown_remaining_ms=0,unexpected_secret='synthetic-credential-NOT-FOR-EXPORT'))
            else:
                profile=next(p for p in profiles if self.path == (f"/api/accounts/{p['id']}/inventory" if p['id']==extra_id else f"/api/{p['id']}-inventory"))
                path=ledger_path(profile)
                with sqlite3.connect(path) as db:
                    state=json.loads(db.execute('SELECT body FROM state').fetchone()[0])
                    transport=json.loads(db.execute('SELECT body FROM market_transport_state').fetchone()[0])
                stamp=int(time.time()*1000)
                state['config'].update(account_max_age_ms=3000,book_max_age_ms=3000)
                accounts=[dict(venue=v,position_units=n,open_orders=0,authenticated=True,observed_ms=stamp,account='synthetic-credential-NOT-FOR-EXPORT') for v,n in zip(['lighter','entropy'],[p['units'] for p in state['positions']])]
                books=[dict(connected=True,received_ms=stamp,bids=[dict(price='2100',units=100)],asks=[dict(price='2100.1',units=200)]) for _ in range(2)]
                risk=dict(estimated_total_pnl='-12.5',depth_complete=False,unseen_exit_units=[300,400],unexpected_secret='synthetic-credential-NOT-FOR-EXPORT')
                value=dict(ok=True,data=dict(profile=profile,csrf='synthetic-credential-NOT-FOR-EXPORT',view=dict(snapshot=state,accounts=accounts,books=books,risk_valuation=risk,market_transport=transport,profit_accounting=dict(funding_complete=False)),notifications=dict(saved=True,unlocked=True,delivery=dict(enabled=True,pending=0,error=None))))
                if profile['id']=='anth':
                    value['data']['view']=None
                    value['data']['startup_failure']=dict(at_ms=stamp,operation='launch',issue=dict(stage='entropy_contract',code='margin_mode_unsupported',message='Entropy margin mode unsupported https://private.invalid/?key=synthetic-credential-NOT-FOR-EXPORT',unexpected_secret='synthetic-credential-NOT-FOR-EXPORT'),unexpected_secret='synthetic-credential-NOT-FOR-EXPORT')
            data=json.dumps(value).encode()
            self.send_response(200)
            self.send_header('Content-Type','application/json')
            self.send_header('Content-Length',str(len(data)))
            self.end_headers()
            self.wfile.write(data)
    server=HTTPServer(('127.0.0.1',19998),Handler)
    (root/'server-ready').touch()
    server.serve_forever()
    sys.exit(0)
if len(sys.argv)>2 and sys.argv[2]=='--hold-lock':
    locks=[]
    for p in root.rglob('*.sqlite'):
        db=sqlite3.connect(p)
        db.execute('BEGIN EXCLUSIVE')
        locks.append(db)
    (root/'lock-ready').touch()
    try:
        time.sleep(90)
    finally:
        for db in locks:
            db.rollback()
            db.close()
    sys.exit(0)
now = int(time.time() * 1000)
secret = "synthetic-credential-MUST-NOT-BE-EXPORTED-0123456789"
(root/'profiles').mkdir(exist_ok=True,parents=True)
(root/'profiles/accounts.json').write_text(json.dumps(dict(schema=1,profiles=profiles)))
for profile in profiles:
    market=profile['market']
    path=ledger_path(profile)
    path.parent.mkdir(parents=True, exist_ok=True)
    start = now - 4 * 3600_000
    request = dict(id="synthetic-order", venue="entropy", side="buy", units=700,
                   limit="2100.10", reduce_only=False, created_ms=start, expires_ms=start+5000)
    pending = dict(id="synthetic-operation", action="open", requested_units=700,
                   created_ms=start, first_venue="entropy", first=request,
                   first_filled=700, hedge_filled=0, repair_filled=0,
                   first_terminal=True, hedge_terminal=True, repair_terminal=False,
                   repair_attempt=2, recovery_slippage_bps=3,
                   entry_hedge_retry=dict(attempt=2,after_ms=now+3000,abandoned=False,reason=f"hedge retry waiting Bearer {secret}",unexpected_secret=secret),
                   recovery_wait_reason=f"depth exceeds protected execution price Bearer {secret}", failed=True)
    state = dict(status="needs_attention", stop_requested=True, paused=True,
                 reason=f"account worker request deadline exceeded https://private.invalid/?key={secret} Bearer {secret}",
                 config=dict(market=market, mode="live", entropy_address="0x"+"a"*40,
                             private_key=secret, grid="5", capital_multiplier=profile.get("capital_multiplier",1), group_notional="150" if profile["id"]==extra_id else "15", leverage=3, max_loss_usdc="300" if profile["id"]==extra_id else "30", accumulation=dict(interval_ms=1800000,max_time_adds=5,quota_scope="grid_stage",unexpected_secret=secret)),
                 pending=pending, positions=[dict(units=-4900),dict(units=5600)],
                 lots=[dict(level=i,units=700,opened_ms=start-10000*(i+1),entry_spread="28.4",private_field=secret) for i in range(7)],
                 fills={"fill-key":dict(id="fill-key",order_id="synthetic-order",venue="entropy",side="buy",units=700,price="2100",fee="0.001",time_ms=start+1000,private_key=secret)},
                 closed_groups=1, direction="lighter_short", anchor="28", time_adds_used=1)
    state['emergency_exit']=dict(id='synthetic-emergency',requested_ms=start,completed_ms=None,flat_confirmed_ms=None,
        remaining_units=[-4900,5600],observed_ms=[now,now],next_attempt_ms=[now,now],next_sequence=1,
        warnings=[f'query failed https://private.invalid/?key={secret}', ''],accounting_error='',
        prior=[dict(request=request,terminal=False)],orders=[dict(request=request,terminal=False)],
        fills=state['fills'],unexpected_secret=secret)
    state['emergency_exit']['account_retry']=[dict(attempts=0,after_ms=now+15000,unexpected_secret=secret)]*2
    for item in state['emergency_exit']['prior']+state['emergency_exit']['orders']:
        item['lookup_retry']=dict(attempts=5,after_ms=now+30000,unexpected_secret=secret)
        item['lookup_note']='Entropy order status=filled requested=700 remaining=500 expected=200 observed=200 rows=1'
    if profile['id']==extra_id:
        state['closed_groups']=99
        state['anchor']='41'
    with sqlite3.connect(path) as db:
        db.executescript("CREATE TABLE state(id INTEGER PRIMARY KEY,body TEXT); CREATE TABLE events(seq INTEGER PRIMARY KEY,at_ms INTEGER,kind TEXT,body TEXT);")
        db.execute("INSERT INTO state VALUES(1,?)", (json.dumps(state),))
        db.execute("CREATE TABLE market_transport_state(id INTEGER PRIMARY KEY,body TEXT)")
        transport=[dict(source='rest',ws_connected=False,reconnect_count=9,ws_books=2,rest_books=60,rest_failures=1,
            last_ws_book_ms=now-9000,last_rest_book_ms=now,last_error_ms=now-1000,
            last_error=f'websocket connect/TLS failed https://private.invalid/?key={secret} Bearer {secret}',
            history=[dict(at_ms=now,kind='ws_reconnect',detail=f'websocket L2 silent for 3000ms Bearer {secret}',unexpected_secret=secret)],
            unexpected_secret=secret) for _ in range(2)]
        db.execute("INSERT INTO market_transport_state VALUES(1,?)",(json.dumps(transport),))
        db.executescript("CREATE TABLE market_notification_state(id INTEGER PRIMARY KEY,body TEXT); CREATE TABLE market_notification_history(seq INTEGER PRIMARY KEY,body TEXT);")
        evidence=dict(at_ms=now,window_ms=60000,observed_ms=60000,unavailable_ms=[50000,0],current_bad=[True,False],eligible=[True,False],received_ms=[now-2000,now],reasons=['stale','healthy'],unexpected_secret=secret)
        db.execute("INSERT INTO market_notification_state VALUES(1,?)", (json.dumps(dict(schema=2,last_sent_ms=now,outbox_id=None,evidence=evidence,unexpected_secret=secret)),))
        db.execute("INSERT INTO market_notification_history VALUES(1,?)", (json.dumps(dict(kind='delivered',at_ms=now,evidence=evidence,unexpected_secret=secret)),))
        for seq, at, kind in [(1,start,"operation_reserved"),(2,start+1000,"order_observed"),(3,now,"reconciliation_blocked")]:
            db.execute("INSERT INTO events VALUES(?,?,?,?)",(seq,at,kind,json.dumps(dict(status=state['status'],reason=state['reason'],pending=pending,private_key=secret,groups=7))))
        recovery=dict(command='"confirm_rollback_recovery"',id=secret,evidence=dict(previous_rollbacks=3,unexpected_secret=secret,
            accounts=[dict(venue=v,position_units=n,open_orders=0,authenticated=True,observed_ms=now,account=secret) for v,n in [('lighter',-4900),('entropy',4900)]]))
        db.execute("INSERT INTO events VALUES(?,?,?,?)",(4,now+1,'command',json.dumps(recovery)))
(root / "vault-secret-MUST-NOT-BE-READ.txt").write_text(secret)
(root / "profile-secret-MUST-NOT-BE-READ.json").write_text(json.dumps(dict(private_key=secret)))
