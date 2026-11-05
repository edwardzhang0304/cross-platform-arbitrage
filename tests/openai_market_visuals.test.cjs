const {test}=require('node:test');
const assert=require('node:assert/strict');
const {chartModel,renderCharts,renderQuoteCards,quoteRows,displayDirection}=require('../frontend/openai-market-visuals.js');
const FIVE=300000,HOUR=3600000,END=Date.UTC(2026,10,6,8,5),NOW=END-180000,START=END-8*HOUR;
const point=(time_ms,spread=7)=>({time_ms,lighter_bid:100+spread,lighter_ask:101+spread,entropy_bid:99,entropy_ask:100});

// Offline SVG harness includes letterboxing, page scaling and the compact view.
function fixture({width=1300,height=245,zoom=1,left=40,top=20}={}) {
  const viewport={width,height,zoom,left,top};
  const matrix=()=>{
    const vb=viewport.width<580?480:1000,s=Math.min(viewport.width/vb,viewport.height/255)*viewport.zoom;
    return {s,tx:viewport.left+(viewport.width*viewport.zoom-vb*s)/2,
      ty:viewport.top+(viewport.height*viewport.zoom-255*s)/2};
  };
  class Element {
    constructor(tag){this.tag=tag;this.attrs={};this.children=[];this.events={};this.textContent='';}
    setAttribute(k,v){this.attrs[k]=String(v);}
    append(...children){this.children.push(...children);}
    replaceChildren(...children){this.children=children;}
    addEventListener(name,fn){(this.events[name]??=[]).push(fn);}
    dispatch(name,event={}){for(const fn of this.events[name]||[])fn(event);}
    getScreenCTM(){return {inverse:()=>{
      const {s,tx,ty}=matrix();return {a:1/s,d:1/s,e:-tx/s,f:-ty/s};
    }};}
    createSVGPoint(){return {x:0,y:0,matrixTransform(m){return {x:this.x*m.a+m.e,y:this.y*m.d+m.f};}};}
  }
  global.document={createElementNS:(_,tag)=>new Element(tag)};
  const root=new Element('div'),status=new Element('p'),hover=new Element('p'),entry=new Element('button'),exit=new Element('button');
  Object.defineProperty(root,'clientWidth',{get:()=>viewport.width});
  Object.defineProperty(root,'clientHeight',{get:()=>viewport.height});
  const get=id=>({'comparison-candles':root,'comparison-chart-status':status,'comparison-chart-hover':hover,
    'comparison-chart-entry':entry,'comparison-chart-exit':exit})[id];
  const start=START+48*FIVE;
  const payload={status:{started_ms:START,observed_ms:NOW},chart_points:[point(start+1000,7),point(start+150000,9),point(start+299000,8),point(NOW,10)]};
  const render=()=>renderCharts(payload,get);
  const cursor=()=>root.children[0].children.find(n=>n.attrs['pointer-events']==='none');
  const move=(x,y=120)=>{const {s,tx,ty}=matrix();root.dispatch('pointermove',{clientX:x*s+tx,clientY:y*s+ty});};
  const candleNodes=()=>root.children[0].children.filter(n=>n.attrs['data-candle']);
  return {root,status,hover,entry,exit,payload,render,cursor,move,viewport,candleNodes};
}
const near=(actual,expected)=>assert.ok(Math.abs(Number(actual)-expected)<1e-8,`${actual} != ${expected}`);

test('96 aligned five-minute slots cover eight hours, including the forming bar',()=>{
  const f=fixture();f.payload.chart_points=Array.from({length:28803},(_,i)=>point(NOW-(28802-i)*1000));
  const model=chartModel(f.payload.chart_points,NOW);
  assert.equal(model.candles.length,96);assert.equal(model.start,START);assert.equal(model.end,END);
  assert.equal(model.end-model.start,8*HOUR);assert.equal(model.rows[0].time_ms,START);
  assert.equal(model.candles.filter(c=>c.forming).length,1);
  assert.equal(model.candles[0].shortL.samples,300);assert.equal(model.candles[0].shortL.incomplete,false);
  assert.equal(model.candles.at(-1).shortL.samples,121);assert.equal(model.candles.at(-1).shortL.incomplete,false);
  assert.doesNotThrow(()=>f.render());assert.equal(f.candleNodes().length,96);
});

test('OHLC aggregates simultaneous spreads, sorts observations and keeps boundary samples in the next candle',()=>{
  const rows=[point(START+FIVE,12),point(START+299000,8),point(START+2000,11),point(START,7),point(START+3000,5)];
  // Independent platform extrema must never be subtracted to synthesize spread extrema.
  rows.push({time_ms:START+4000,lighter_bid:210,lighter_ask:211,entropy_bid:199,entropy_ask:200});
  const model=chartModel(rows,NOW),bar=model.candles[0].shortL;
  assert.deepEqual([bar.open,bar.high,bar.low,bar.close],[7,11,5,8]);
  assert.deepEqual(['open','high','low','close'].map(k=>model.candles[0].closeL[k]),[9,13,7,10]);
  assert.equal(model.candles[1].shortL.open,12);assert.equal(model.candles[1].shortL.samples,1);
  assert.equal(model.candles[0].shortL.incomplete,true);
});

test('sparse, invalid and future observations never invent candles or backfill gaps',()=>{
  const rows=[point(START-1),point(START),point(START+6*FIVE,9),point(NOW+1,100),point(END+FIVE,200)];
  const model=chartModel(rows,NOW);
  assert.equal(model.start,START);assert.equal(model.rows.length,2);
  assert.equal(model.candles.filter(c=>c.shortL).length,2);assert.equal(model.candles[1].shortL,null);
  assert.equal(model.candles.at(-1).shortL,null);
  assert.equal(chartModel([point(START-1)],NOW),null);
  assert.equal(chartModel([point(NOW+1)],NOW),null);
  const f=fixture();f.payload.chart_points=rows;f.render();f.move(519);
  assert.match(f.hover.textContent,/无有效报价$/);assert.equal(f.candleNodes().length,2);
  const invalid={time_ms:START+1000,lighter_bid:110,lighter_ask:109,entropy_bid:99,entropy_ask:100};
  const partial=chartModel([point(START),invalid,point(START+2000)],NOW).candles[0];
  assert.equal(partial.shortL.samples,2);assert.equal(partial.shortL.incomplete,true);
  assert.equal(partial.closeL.incomplete,true);
  const absent=chartModel([invalid],NOW);assert.equal(absent.candles[0].shortL,null);
});

test('forming bar updates within its slot and rolls once at the exact five-minute boundary',()=>{
  const before=chartModel([point(NOW),point(END-1,11)],END-1);
  const after=chartModel([point(NOW),point(END-1,11),point(END,12)],END);
  assert.equal(before.candles.at(-1).shortL.close,11);
  assert.equal(after.start,before.start+FIVE);assert.equal(after.candles.length,96);
  assert.equal(after.candles.at(-2).forming,false);assert.equal(after.candles.at(-2).shortL.close,11);
  assert.equal(after.candles.at(-1).forming,true);assert.equal(after.candles.at(-1).shortL.open,12);
});

test('stale Entropy remains visible while fresh Lighter is labeled independently',()=>{
  const now=1700000000000;
  const payload={available:true,symbols:['ANTHROPIC','io:ANTH'],status:{running:true,observed_ms:now,
    books:[now,now-15000].map(received_ms=>({bid:{price:2000},ask:{price:2001},received_ms,connected:true}))}};
  const quotes=quoteRows(payload,now,true);
  assert.deepEqual(quotes.map(q=>q.state),['行情有效','报价过期']);
  const element=()=>({children:[],textContent:'',append(...children){this.children.push(...children)},replaceChildren(){this.children=[]}});
  global.document={createElement:element}; const root=element();
  renderQuoteCards(quotes,'shortL',()=>root);
  assert.deepEqual(root.children.map(r=>r.children[3].textContent),['行情有效','报价过期']);
  assert.equal(root.children[1].children[3].className,'quote-state quote-stale');
  assert.match(root.children[1].children[3].title,/最近盘口/);
  assert(quoteRows(payload,now,false).every(q=>!q.fresh));
});

test('chart explains missing latest quotes and clears the message on recovery',()=>{
  const f=fixture();f.payload.chart_points.at(-1).entropy_bid=null;
  f.payload.chart_points.at(-1).entropy_ask=null;
  f.render();assert.match(f.status.textContent,/行情缺失或过期/);
  f.payload.chart_points.at(-1).entropy_bid=99;f.payload.chart_points.at(-1).entropy_ask=100;
  f.render();assert.equal(f.status.textContent,'');
  f.payload.chart_points=[];f.render();assert.equal(f.status.textContent,'等待有效行情');
});

test('delayed live transport is distinguished from fresh trading prices and disconnection',()=>{
  const now=1700000000000;
  const payload={available:true,status:{running:true,observed_ms:now,books:[{},{}],market_transport:[{},
    {ws_connected:true,last_ws_message_ms:now,last_ws_source_age_ms:6000}]}};
  let quote=quoteRows(payload,now,true)[1];
  assert.equal(quote.state,'行情延迟 · 约 6.0 秒');assert.equal(quote.buy,'—');assert.equal(quote.fresh,false);
  payload.status.books[1]={bid:{price:2000},ask:{price:2001},received_ms:now-60_000,connected:true};
  quote=quoteRows(payload,now,true)[1];assert.equal(quote.state,'行情延迟 · 约 6.0 秒');assert.equal(quote.fresh,false);
  payload.status.books[1].received_ms=now-200; // Independently fresh REST may bridge delayed WS.
  assert.equal(quoteRows(payload,now,true)[1].fresh,true);
  payload.status.books[1].received_ms=now-60_000;
  assert.equal(quoteRows(payload,now,false)[1].state,'连接中断 · 旧报价');
  payload.status.market_transport[1].ws_connected=false;
  assert.equal(quoteRows(payload,now,true)[1].state,'报价过期');
  payload.status.market_transport[1]={ws_connected:true,last_ws_message_ms:now,last_ws_source_age_ms:200};
  payload.status.books[1].received_ms=now-200;
  assert.equal(quoteRows(payload,now,true)[1].fresh,true);
});

test('quote cards keep Lighter then Entropy when the trading direction changes',()=>{
  const element=()=>({children:[],textContent:'',append(...children){this.children.push(...children)},replaceChildren(){this.children=[]}});
  const root=element();
  global.document={createElement:element};
  const quotes=[{platform:'Lighter RH',symbol:'ANTHROPIC',buy:'2110.00',sell:'2109.90'},
    {platform:'Entropy',symbol:'io:ANTH',buy:'2073.40',sell:'2073.20'}];
  for(const direction of ['shortL','shortE','shortL']){
    renderQuoteCards(quotes,direction,()=>root);
    assert.deepEqual(root.children.map(row=>row.children[0].textContent),['Lighter RH','Entropy']);
    assert.deepEqual(root.children.map(row=>row.children[2].textContent),['2109.90','2073.20']);
  }
});

test('cursor uses SVG coordinates across wide, tall, zoomed and compact layouts',()=>{
  for(const box of [{width:1300},{width:580,height:500},{width:930,zoom:1.75,left:-120,top:80},{width:360}]){
    const f=fixture(box),x=box.width<580?259:519;f.render();f.move(x);
    const scale=Math.min(f.viewport.width/(box.width<580?480:1000),f.viewport.height/255);
    for(const axis of f.root.children[0].children.filter(n=>n.attrs['data-axis']))near(Number(axis.attrs['font-size'])*scale,14);
    near(f.cursor().attrs.x1,x);assert.equal(f.cursor().attrs.visibility,'visible');
    assert.match(f.hover.textContent,/Bid−Ask · 开 7.0000  高 9.0000  低 7.0000  收 8.0000/);
    f.move(x+1);near(f.cursor().attrs.x1,x+1);
  }
});

test('series switch preserves direction, OHLC and selection across refreshes with one listener',()=>{
  for(const reverse of [false,true]){
    const f=fixture();if(reverse)for(const p of f.payload.chart_points){
      [p.lighter_bid,p.entropy_bid]=[p.entropy_bid,p.lighter_bid];
      [p.lighter_ask,p.entropy_ask]=[p.entropy_ask,p.lighter_ask];
    }
    const original=JSON.stringify(f.payload);f.render();f.move(519);
    assert.equal(displayDirection(f.payload),reverse?'shortE':'shortL');
    assert.equal(f.candleNodes()[0].attrs['data-series'],reverse?'shortE':'shortL');
    assert.equal(f.entry.attrs['aria-pressed'],'true');f.exit.dispatch('click');
    assert.equal(f.candleNodes()[0].attrs['data-series'],reverse?'closeE':'closeL');
    assert.equal(f.exit.attrs['aria-pressed'],'true');assert.equal(f.entry.attrs['aria-pressed'],'false');
    assert.match(f.hover.textContent,/Ask−Bid · 开 9.0000  高 11.0000  低 9.0000  收 10.0000/);
    for(let i=0;i<10;i++)f.render();
    assert.equal(f.exit.events.click.length,1);assert.equal(f.root.events.pointermove.length,1);
    assert.equal(f.exit.attrs['aria-pressed'],'true');assert.equal(JSON.stringify(f.payload),original);
  }
});

test('a stationary cursor updates OHLC after refresh without snapping to other buckets',()=>{
  const f=fixture();f.render();f.move(519);const old=f.cursor();
  f.payload.chart_points[1].lighter_bid=110;f.render();
  assert.notEqual(f.cursor(),old);near(f.cursor().attrs.x1,519);
  assert.match(f.hover.textContent,/高 10.0000/);
  f.move(742);near(f.cursor().attrs.x1,742);assert.match(f.hover.textContent,/无有效报价$/);
});

test('leaving the plot, cancelling or losing history clears the readout',()=>{
  const f=fixture();f.render();
  for(const [x,y] of [[60,120],[970,120],[519,30],[519,235]]){
    f.move(519);f.move(x,y);assert.equal(f.cursor().attrs.visibility,'hidden');assert.equal(f.hover.textContent,'');
  }
  for(const event of ['pointerleave','pointercancel']){
    f.move(519);f.root.dispatch(event);f.render();assert.equal(f.cursor().attrs.visibility,'hidden');
  }
  f.move(519);f.viewport.left-=30;f.root.dispatch('scroll');near(f.cursor().attrs.x1,519+30/(245/255));
  f.viewport.width=1500;f.render();near(f.cursor().attrs.x1,519-70/(245/255));
  f.payload.chart_points=[];f.render();assert.equal(f.root.children.length,0);assert.equal(f.hover.textContent,'');
});

test('candle bodies encode rise/fall and high-low wicks stay within the scaled plot',()=>{
  const f=fixture();f.payload.chart_points=[point(START,-3),point(START+299000,-1),point(START+FIVE,9),point(START+2*FIVE-1,5),point(NOW,10)];
  f.render();const candles=f.candleNodes();
  assert.deepEqual(candles.map(g=>g.children[1].attrs.fill),['#139b55','#e24444','#139b55']);
  for(const g of candles){
    const wick=g.children[0],body=g.children[1];assert(Number(body.attrs.height)>=1);
    for(const k of ['y1','y2'])assert(Number(wick.attrs[k])>=62&&Number(wick.attrs[k])<=216);
  }
  assert.equal(f.root.children[0].children.filter(n=>n.tag==='path').length,0);
});

test('raw spreads preserve precision and exclude execution buffers',()=>{
  const p={time_ms:NOW,lighter_bid:1697,lighter_ask:1697.15,entropy_bid:1690.1,entropy_ask:1690.6};
  const row=chartModel([p],NOW).candles.at(-1);
  near(row.shortL.open,6.4);near(row.closeL.open,7.05);near(row.shortE.open,-7.05);near(row.closeE.open,-6.4);
});

test('candle scale fits narrow entry and exit ranges with 5U intervals and no forced zero',()=>{
  const f=fixture();
  f.payload.chart_points=[point(START,45.8),point(START+1000,51.65),point(NOW,50.2)];
  const before=JSON.stringify(f.payload);f.render();
  const ticks=()=>f.root.children[0].children.filter(n=>n.attrs['data-axis']==='y').map(n=>n.textContent);
  // Entry high 51.65, closing high 53.65: padded/rounded to 45..55.
  assert.deepEqual(ticks(),['45','50','55']);
  f.exit.dispatch('click');assert.deepEqual(ticks(),['45','50','55']);
  assert.equal(JSON.stringify(f.payload),before);
  // Shrink again as old extremes leave the window, without a service restart.
  f.payload.chart_points=[point(NOW,10)];f.render();assert.deepEqual(ticks(),['10','15']);
  f.payload.chart_points=[point(NOW,60)];f.render();assert.deepEqual(ticks(),['60','65']);
  f.payload.chart_points=[point(NOW,70)];f.render();assert.deepEqual(ticks(),['70','75']);
});

test('candle scale covers negative spreads and zero without sharing ranges between chart roots',()=>{
  const f=fixture();f.payload.chart_points=[point(START,-11),point(NOW,10)];f.render();
  const ticks=f.root.children[0].children.filter(n=>n.attrs['data-axis']==='y').map(n=>Number(n.textContent));
  assert.deepEqual(ticks,[-15,-10,-5,0,5,10,15]);
  for(const g of f.candleNodes())for(const k of ['y1','y2'])assert(Number(g.children[0].attrs[k])>=62&&Number(g.children[0].attrs[k])<=216);
  const other=fixture();other.payload.chart_points=[point(NOW,2)];other.render();
  assert.equal(other.root.children[0].children.filter(n=>n.attrs['data-axis']==='y').at(-1).textContent,'5');
});

test('automatic candle ticks depend on spread range, not absolute price or market',()=>{
  for(const [low,high,step] of [[50,50,5],[-52,-46,5],[180,198,5],[100,218,20],[10,70,20],[100,430,100]]){
    const f=fixture();f.payload.status.books=[{bid:{price:110},ask:{price:111}},{bid:{price:99},ask:{price:100}}];
    f.payload.chart_points=[point(START,low),point(NOW,high)];f.render();
    const ticks=f.root.children[0].children.filter(n=>n.attrs['data-axis']==='y').map(n=>Number(n.textContent));
    assert(ticks[0]<=low&&ticks.at(-1)>=high+2);
    assert(ticks.length<=7);
    if(step>5)assert(ticks.length>=5);
    for(let i=1;i<ticks.length;i++)assert.equal(ticks[i]-ticks[i-1],step);
    if(high<0)assert(ticks.at(-1)<0);
    for(const g of f.candleNodes())for(const k of ['y1','y2'])assert(Number(g.children[0].attrs[k])>62&&Number(g.children[0].attrs[k])<216);
  }
});
