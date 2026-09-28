const {test}=require('node:test');
const assert=require('node:assert/strict');
const {chartModel,renderCharts,renderQuoteCards}=require('../frontend/openai-market-visuals.js');

// Offline SVG harness: model the browser's xMidYMid/meet transform, including
// letterboxing and CSS/page scaling. No live API, credentials or trading state.
function fixture({width=1300,height=186,zoom=1,left=40,top=20}={}) {
  const viewport={width,height,zoom,left,top};
  const matrix=()=>{
    const s=Math.min(viewport.width/1000,viewport.height/255)*viewport.zoom;
    return {s,tx:viewport.left+(viewport.width*viewport.zoom-1000*s)/2,
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
  const root=new Element('div'),status=new Element('p'),hover=new Element('p');
  const get=id=>({'comparison-candles':root,'comparison-chart-status':status,'comparison-chart-hover':hover})[id];
  const start=1700000000000;
  const payload={status:{started_ms:start},chart_points:[0,150000,300000].map((delta,i)=>({
    time_ms:start+delta,lighter_bid:107+i,lighter_ask:108+i,entropy_bid:99,entropy_ask:100
  }))};
  const render=()=>renderCharts(payload,get);
  const cursor=()=>root.children[0].children.find(n=>n.attrs['pointer-events']==='none');
  const move=(x,y=120)=>{const {s,tx,ty}=matrix();root.dispatch('pointermove',{clientX:x*s+tx,clientY:y*s+ty});};
  return {root,hover,payload,render,cursor,move,viewport};
}
const near=(actual,expected)=>assert.ok(Math.abs(Number(actual)-expected)<1e-8,`${actual} != ${expected}`);

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

test('cursor uses actual SVG coordinates in wide/tall/scaled viewports',()=>{
  for(const box of [{width:1300},{width:580,height:500},{width:930,zoom:1.75,left:-120,top:80}]){
    const f=fixture(box);f.render();f.move(519);
    near(f.cursor().attrs.x1,519);near(f.cursor().attrs.x2,519);
    assert.equal(f.cursor().attrs.visibility,'visible');
    assert.match(f.hover.textContent,/Bid−Ask 8\.0000 · Ask−Bid 10\.0000$/);
    f.move(742);near(f.cursor().attrs.x1,742);
    f.move(743);near(f.cursor().attrs.x1,743); // No jump to a distant/sparse sample.
  }
});

test('refresh preserves the stationary pointer and updates its quote',()=>{
  const f=fixture();f.render();f.move(519);
  const old=f.cursor();f.payload.chart_points[1].lighter_bid=107.5;
  f.render();assert.notEqual(f.cursor(),old);
  near(f.cursor().attrs.x1,519);assert.equal(f.cursor().attrs.visibility,'visible');
  assert.match(f.hover.textContent,/Bid−Ask 7\.5000 · Ask−Bid 10\.0000$/);
  for(let i=0;i<10;i++)f.render();
  assert.equal(f.root.events.pointermove.length,1);
  f.move(740);near(f.cursor().attrs.x1,740);
});

test('leaving the plot, leaving the container or cancelling clears cursor and readout',()=>{
  const f=fixture();f.render();
  for(const [x,y] of [[60,120],[970,120],[519,30],[519,235]]){
    f.move(519);f.move(x,y);
    assert.equal(f.cursor().attrs.visibility,'hidden');assert.equal(f.hover.textContent,'');
  }
  for(const event of ['pointerleave','pointercancel']){
    f.move(519);f.root.dispatch(event);f.render();
    assert.equal(f.cursor().attrs.visibility,'hidden');assert.equal(f.hover.textContent,'');
  }
});

test('scroll/resize use the current transform; empty data does not leave a stale readout',()=>{
  const f=fixture();f.render();f.move(519);
  f.viewport.left-=30;f.root.dispatch('scroll');
  near(f.cursor().attrs.x1,519+30/(186/255));
  f.viewport.width=1500;f.render();
  near(f.cursor().attrs.x1,519-70/(186/255));
  f.payload.chart_points=[];f.render();
  assert.equal(f.root.children.length,0);assert.equal(f.hover.textContent,'');
  f.root.dispatch('pointerleave');
});

test('green bid-minus-ask and red ask-minus-bid use the same direction',()=>{
  for(const reverse of [false,true]){
    const f=fixture();
    if(reverse)for(const p of f.payload.chart_points){
      [p.lighter_bid,p.entropy_bid]=[p.entropy_bid,p.lighter_bid];
      [p.lighter_ask,p.entropy_ask]=[p.entropy_ask,p.lighter_ask];
    }
    f.render();f.move(519);
    const paths=f.root.children[0].children.filter(n=>n.tag==='path');
    assert.deepEqual(paths.map(p=>[p.attrs['data-series'],p.attrs.stroke]),
      [[reverse?'shortE':'shortL','#139b55'],[reverse?'closeE':'closeL','#e24444']]);
    assert.match(f.hover.textContent,/Bid−Ask 8\.0000 · Ask−Bid 10\.0000$/);
    for(const path of paths)for(const match of path.attrs.d.matchAll(/[ML][\d.]+,([\d.]+)/g)){
      assert(Number(match[1])>=62&&Number(match[1])<=216,'both series fit the plot');
    }
  }
});

test('both raw spreads exclude execution buffers and preserve quote gaps',()=>{
  const f=fixture();const t=f.payload.chart_points[0].time_ms;
  const quote={lighter_bid:1697,lighter_ask:1697.15,entropy_bid:1690.1,entropy_ask:1690.6};
  f.payload.chart_points=Array.from({length:5},(_,i)=>({time_ms:t+i*1000,...quote}));
  f.payload.chart_points[2].lighter_ask=null;
  f.payload.chart_points[2].lighter_bid=null;
  const rows=chartModel(f.payload.chart_points).rows;
  near(rows[0].shortL,6.4);near(rows[0].closeL,7.05);
  near(rows[0].shortE,-7.05);near(rows[0].closeE,-6.4);
  assert.equal(rows[2].shortL,null);assert.equal(rows[2].closeL,null);
  f.render();
  for(const path of f.root.children[0].children.filter(n=>n.tag==='path')){
    assert.equal((path.attrs.d.match(/M/g)||[]).length,2);
  }
  f.payload.chart_points[0].lighter_bid=1700; // Crossed book is invalid for both curves.
  const invalid=chartModel(f.payload.chart_points).rows[0];
  assert.equal(invalid.shortL,null);assert.equal(invalid.closeL,null);
});
