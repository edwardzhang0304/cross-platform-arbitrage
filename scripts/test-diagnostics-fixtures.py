"""Synthetic private-shaped inputs; never reads accounts or user data."""
import json
import pathlib
import sqlite3
import sys
import time

root = pathlib.Path(sys.argv[1])
if len(sys.argv)>2 and sys.argv[2]=='--serve':
    from http.server import BaseHTTPRequestHandler, HTTPServer
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass
        def do_GET(self):
            if self.path=='/health':
                value=dict(application='openai-paired-trader',version='synthetic',data_dir=str(root),build=dict(source_commit='synthetic'))
            else:
                market='anth' if 'anth' in self.path else 'openai'
                path=root/('profiles/anth-live/runtime/inventory.sqlite' if market=='anth' else 'runtime/openai-inventory/live.sqlite')
                with sqlite3.connect(path) as db:
                    state=json.loads(db.execute('SELECT body FROM state').fetchone()[0])
                stamp=int(time.time()*1000)
                state['config'].update(account_max_age_ms=3000,book_max_age_ms=3000)
                accounts=[dict(venue=v,position_units=n,open_orders=0,authenticated=True,observed_ms=stamp,account='synthetic-credential-NOT-FOR-EXPORT') for v,n in [('lighter',-4900),('entropy',5600)]]
                books=[dict(connected=True,received_ms=stamp,bids=[dict(price='2100')],asks=[dict(price='2100.1')]) for _ in range(2)]
                value=dict(ok=True,data=dict(csrf='synthetic-credential-NOT-FOR-EXPORT',view=dict(snapshot=state,accounts=accounts,books=books,profit_accounting=dict(funding_complete=False)),notifications=dict(saved=True,unlocked=True,delivery=dict(enabled=True,pending=0,error=None))))
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
for market, path in [
    ("openai", root / "runtime/openai-inventory/live.sqlite"),
    ("anth", root / "profiles/anth-live/runtime/inventory.sqlite"),
]:
    path.parent.mkdir(parents=True, exist_ok=True)
    start = now - 4 * 3600_000
    request = dict(id="synthetic-order", venue="entropy", side="buy", units=700,
                   limit="2100.10", reduce_only=False, created_ms=start, expires_ms=start+5000)
    pending = dict(id="synthetic-operation", action="open", requested_units=700,
                   created_ms=start, first_venue="entropy", first=request,
                   first_filled=700, hedge_filled=0, repair_filled=0,
                   first_terminal=True, hedge_terminal=True, repair_terminal=False,
                   repair_attempt=0, failed=True)
    state = dict(status="needs_attention", stop_requested=True, paused=True,
                 reason=f"account worker request deadline exceeded https://private.invalid/?key={secret} Bearer {secret}",
                 config=dict(market=market, mode="live", entropy_address="0x"+"a"*40,
                             private_key=secret, grid="5", accumulation=dict(interval_ms=1800000,max_time_adds=5,quota_scope="grid_stage",unexpected_secret=secret)),
                 pending=pending, positions=[dict(units=-4900),dict(units=5600)],
                 lots=[dict(level=i,units=700,opened_ms=start-10000*(i+1),entry_spread="28.4",private_field=secret) for i in range(7)],
                 fills={"fill-key":dict(id="fill-key",order_id="synthetic-order",venue="entropy",side="buy",units=700,price="2100",fee="0.001",time_ms=start+1000,private_key=secret)},
                 closed_groups=1, direction="lighter_short", anchor="28", time_adds_used=1)
    with sqlite3.connect(path) as db:
        db.executescript("CREATE TABLE state(id INTEGER PRIMARY KEY,body TEXT); CREATE TABLE events(seq INTEGER PRIMARY KEY,at_ms INTEGER,kind TEXT,body TEXT);")
        db.execute("INSERT INTO state VALUES(1,?)", (json.dumps(state),))
        for seq, at, kind in [(1,start,"operation_reserved"),(2,start+1000,"order_observed"),(3,now,"reconciliation_blocked")]:
            db.execute("INSERT INTO events VALUES(?,?,?,?)",(seq,at,kind,json.dumps(dict(status=state['status'],reason=state['reason'],pending=pending,private_key=secret,groups=7))))
(root / "vault-secret-MUST-NOT-BE-READ.txt").write_text(secret)
(root / "profile-secret-MUST-NOT-BE-READ.json").write_text(json.dumps(dict(private_key=secret)))
