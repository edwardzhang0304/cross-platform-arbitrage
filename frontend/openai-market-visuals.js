/* Read-only live chart and quotes. No strategy controls or polling. */
(function () {
  'use strict';
  const CHART_WINDOW_MS=8*60*60*1000;
  const CANDLE_MS=5*60*1000;
  const SPREAD_KEYS=['shortL','shortE','closeL','closeE'];
  const stamp = t => t ? new Date(t).toLocaleString('zh-CN', {hour12:false}) : '—';
  const quotePrice = n => n == null || !Number.isFinite(Number(n)) || Number(n)<=0 ? '—' : Number(n).toLocaleString('en-US',{minimumFractionDigits:2,maximumFractionDigits:4});
  function chartModel(raw,observedMs) {
    const price=v=>v==null||!Number.isFinite(Number(v))||Number(v)<=0?null:Number(v);
    let rows=(Array.isArray(raw)?raw:[]).map(p=>{
      let lb=price(p.lighter_bid),la=price(p.lighter_ask),eb=price(p.entropy_bid),ea=price(p.entropy_ask);
      if(lb!=null&&la!=null&&lb>la){lb=null;la=null;}
      if(eb!=null&&ea!=null&&eb>ea){eb=null;ea=null;}
      return {time_ms:Number(p.time_ms),lighter:lb!=null&&la!=null?(lb+la)/2:null,
        entropy:eb!=null&&ea!=null?(eb+ea)/2:null,
        shortL:lb!=null&&ea!=null?lb-ea:null,shortE:eb!=null&&la!=null?eb-la:null,
        closeL:la!=null&&eb!=null?la-eb:null,closeE:ea!=null&&lb!=null?ea-lb:null};
    }).filter(p=>Number.isSafeInteger(p.time_ms)&&p.time_ms>0).sort((a,b)=>a.time_ms-b.time_ms);
    if(!rows.length)return null;
    const now=Number.isSafeInteger(observedMs)&&observedMs>0?observedMs:rows.at(-1).time_ms;
    // Exactly 96 wall-clock five-minute slots, including the forming candle.
    const end=Math.floor(now/CANDLE_MS)*CANDLE_MS+CANDLE_MS,start=end-CHART_WINDOW_MS;
    rows=rows.filter(p=>p.time_ms>=start&&p.time_ms<=now);
    if(!rows.length)return null;
    const candles=Array.from({length:CHART_WINDOW_MS/CANDLE_MS},(_,i)=>({
      time_ms:start+i*CANDLE_MS,forming:start+(i+1)*CANDLE_MS>now,
      ...Object.fromEntries(SPREAD_KEYS.map(k=>[k,null]))
    }));
    for(const row of rows){
      const candle=candles[Math.floor((row.time_ms-start)/CANDLE_MS)];
      for(const key of SPREAD_KEYS){
        const v=row[key];if(v==null)continue;
        let bar=candle[key];
        if(!bar)bar=candle[key]={open:v,high:v,low:v,close:v,first_ms:row.time_ms,last_ms:row.time_ms,samples:0,incomplete:false};
        bar.incomplete ||= row.time_ms-bar.last_ms>2500;
        bar.high=Math.max(bar.high,v);bar.low=Math.min(bar.low,v);bar.close=v;
        bar.last_ms=row.time_ms;bar.samples++;
      }
    }
    // Null observations are gaps, never zeroes or forward-filled prices. A
    // partially observed candle remains useful but is explicitly marked.
    for(const row of rows)for(const key of SPREAD_KEYS){
      const bar=candles[Math.floor((row.time_ms-start)/CANDLE_MS)][key];
      if(bar&&row[key]==null)bar.incomplete=true;
    }
    for(const candle of candles)for(const key of SPREAD_KEYS){
      const bar=candle[key];if(!bar)continue;
      bar.incomplete ||= bar.first_ms-candle.time_ms>2500||Math.min(candle.time_ms+CANDLE_MS,now)-bar.last_ms>2500;
    }
    return {rows,candles,start,end,now};
  }
  function displayDirection(payload) {
    const b=payload?.status?.books||[],n=x=>Number(x);
    const values=[b[0]?.bid?.price,b[0]?.ask?.price,b[1]?.bid?.price,b[1]?.ask?.price];
    if(values.every(x=>x!=null&&Number.isFinite(n(x))&&n(x)>0)&&n(values[0])<=n(values[1])&&n(values[2])<=n(values[3]))
      return n(values[0])-n(values[3])>=n(values[2])-n(values[1])?'shortL':'shortE';
    const row=chartModel(payload?.chart_points)?.rows.slice().reverse().find(r=>r.shortL!=null&&r.shortE!=null);
    return !row||row.shortL>=row.shortE?'shortL':'shortE';
  }
  function spreadAxis(candles,keys) {
    // Fit the current eight-hour range with 5/10/20/50/... ticks, up to six
    // intervals. Keep label rounding separate from the small visual padding.
    let lo=Infinity,hi=-Infinity;
    for(const candle of candles)for(const key of keys)if(candle[key]){
      lo=Math.min(lo,candle[key].low);hi=Math.max(hi,candle[key].high);
    }
    if(!Number.isFinite(lo)||!Number.isFinite(hi)){lo=0;hi=5;}
    const target=Math.max(5,(hi-lo)/6),power=10**Math.floor(Math.log10(target));
    let step=[1,2,5,10].find(n=>n*power>=target)*power;
    let tickMin,tickMax;
    for(;;){
      tickMin=Math.floor(lo/step)*step;tickMax=Math.ceil(hi/step)*step;
      if((tickMax-tickMin)/step<=6)break;
      const unit=10**Math.floor(Math.log10(step)),factor=Math.round(step/unit);
      step=(factor===1?2:factor===2?5:10)*unit;
    }
    if(tickMin===tickMax){tickMin-=step;tickMax+=step;}
    // Larger scales can use four intervals without magnifying tiny changes.
    if(step>5&&(tickMax-tickMin)/step<4){
      tickMin=Math.round(((lo+hi)/2-2*step)/step)*step;
      tickMax=tickMin+4*step;
    }
    return {min:tickMin-.25,max:tickMax+.25,tickMin,tickMax,step};
  }
  const chartPointers=new WeakMap();
  function pointerState(root) {
    let state=chartPointers.get(root);
    if(!state){
      state={point:null,draw:null,series:'entry',buttons:new WeakSet()};
      chartPointers.set(root,state);
      // Keep listeners on the container: quote refreshes replace the SVG below it.
      root.addEventListener('pointermove',e=>{
        state.point={x:e.clientX,y:e.clientY};state.draw?.();
      });
      root.addEventListener('pointerleave',()=>{state.point=null;state.draw?.();});
      root.addEventListener('pointercancel',()=>{state.point=null;state.draw?.();});
      root.addEventListener('scroll',()=>state.draw?.());
    }
    return state;
  }
  function renderCharts(payload,get) {
    const root=get('comparison-candles'),status=get('comparison-chart-status'),hover=get('comparison-chart-hover');
    if(!root||!status||!document.createElementNS)return;
    const pointer=pointerState(root);pointer.draw=null;
    pointer.render=()=>renderCharts(payload,get);
    for(const series of ['entry','exit']){
      const button=get(`comparison-chart-${series}`);if(!button)continue;
      button.setAttribute('aria-pressed',String(pointer.series===series));
      if(!pointer.buttons.has(button)){
        pointer.buttons.add(button);
        button.addEventListener('click',()=>{pointer.series=series;pointer.render();});
      }
    }
    root.replaceChildren();status.textContent='';if(hover)hover.textContent='';
    const model=chartModel(payload?.chart_points,payload?.status?.observed_ms);
    if(!model){status.textContent='等待有效行情';return;}
    const node=(tag,attrs={},text)=>{const n=document.createElementNS('http://www.w3.org/2000/svg',tag);
      for(const [k,v] of Object.entries(attrs))n.setAttribute(k,String(v));if(text!=null)n.textContent=text;return n;};
    const compact=root.clientWidth>0&&root.clientWidth<580;
    // Keep axis labels readable in CSS pixels even when SVG meet-scaling adds
    // letterboxing. A nominal 12px SVG font used to shrink below 10px here.
    const scale=Math.min((root.clientWidth||1000)/(compact?480:1000),(root.clientHeight||255)/255);
    const axisFont=14/scale,captionFont=13/scale;
    const label=pointer.series==='entry'?'开仓价差 · Bid−Ask':'平仓价差 · Ask−Bid';
    const svg=node('svg',{viewBox:`0 0 ${compact?480:1000} 255`,role:'img','aria-label':`${label} · 5 分钟蜡烛图 · 最近 8 小时`,style:'user-select:none;-webkit-user-select:none'});
    svg.append(node('title',{},`${label}；绿涨红跌；每根为五分钟内实际价差的开高低收`));
    const left=compact?62:74,width=compact?390:890,height=154,x=t=>left+(t-model.start)/(model.end-model.start)*width;
    const selected=displayDirection(payload);
    const closing=selected==='shortL'?'closeL':'closeE';
    const key=pointer.series==='entry'?selected:closing;
    const range=spreadAxis(model.candles,[selected,closing]);
    const latest=model.rows.at(-1),value=v=>v==null?'—':v.toFixed(4);
    if(latest[selected]==null || latest[closing]==null)status.textContent='行情缺失或过期，价差暂停更新';
    svg.append(node('text',{x:left,y:25,fill:'#526277','font-size':captionFont},selected==='shortL'?'空 Lighter / 多 Entropy':'空 Entropy / 多 Lighter'));
    const panels=[{top:62,range}];
    for(const panel of panels){
      const {top,range}=panel,y=v=>top+height-(v-range.min)/(range.max-range.min)*height;
      svg.append(node('text',{x:left,y:top-15,fill:'#526277','font-size':captionFont},`${label}  ${value(latest[key])}`));
      for(let i=0;i<=Math.round((range.tickMax-range.tickMin)/range.step);i++){
        const p=range.tickMin+range.step*i,yy=y(p);
        svg.append(node('line',{x1:left,x2:left+width,y1:yy,y2:yy,stroke:'#e2e8f0'}),
          node('text',{x:left-8,y:yy+axisFont*.32,'text-anchor':'end',fill:'#526277','font-size':axisFont,'font-weight':500,'data-axis':'y'},p.toFixed(0)));
      }
      if(range.min<0&&range.max>0)svg.append(node('line',{x1:left,x2:left+width,y1:y(0),y2:y(0),stroke:'#64748b','stroke-dasharray':'4 4'}));
      for(let i=0;i<=4;i++){
        const t=model.start+(model.end-model.start)*i/4,xx=x(t);
        svg.append(node('line',{x1:xx,x2:xx,y1:top,y2:top+height,stroke:'#f1f5f9'}),
          node('text',{x:xx,y:top+height+22,'text-anchor':i===0?'start':i===4?'end':'middle',fill:'#526277','font-size':axisFont,'data-axis':'x'},new Date(t).toLocaleTimeString('zh-CN',{hour:'2-digit',minute:'2-digit',hour12:false})));
      }
      const bodyWidth=width/model.candles.length*.65;
      for(const candle of model.candles){
        const bar=candle[key];if(!bar)continue;
        const cx=x(candle.time_ms+CANDLE_MS/2),color=bar.close>=bar.open?'#139b55':'#e24444';
        const group=node('g',{'data-series':key,'data-candle':candle.time_ms,'data-forming':candle.forming,'data-incomplete':bar.incomplete});
        group.append(node('line',{x1:cx,x2:cx,y1:y(bar.high),y2:y(bar.low),stroke:color,'stroke-width':1}),
          node('rect',{x:cx-bodyWidth/2,y:Math.min(y(bar.open),y(bar.close))-.5,width:bodyWidth,
            height:Math.max(1,Math.abs(y(bar.open)-y(bar.close))),fill:color,rx:.4}));
        group.append(node('title',{},`${stamp(candle.time_ms)} · ${label} · 开 ${value(bar.open)} 高 ${value(bar.high)} 低 ${value(bar.low)} 收 ${value(bar.close)}${candle.forming?' · 形成中':''}${bar.incomplete?' · 含行情缺口':''}`));
        svg.append(group);
      }
    }
    const cursors=panels.map(p=>node('line',{x1:left,x2:left,y1:p.top,y2:p.top+height,stroke:'#94a3b8','stroke-dasharray':'3 3',visibility:'hidden','pointer-events':'none'}));
    svg.append(...cursors);
    pointer.draw=()=>{
        const hide=()=>{for(const cursor of cursors)cursor.setAttribute('visibility','hidden');if(hover)hover.textContent='';};
        if(!pointer.point){hide();return;}
        // The SVG is centered inside a fixed-height viewport. Its screen matrix
        // includes that padding, page zoom and scrolling; width alone does not.
        const matrix=svg.getScreenCTM();
        if(!matrix){hide();return;}
        const point=svg.createSVGPoint();point.x=pointer.point.x;point.y=pointer.point.y;
        let local;
        try{local=point.matrixTransform(matrix.inverse());}catch{hide();return;}
        const xx=local.x,yy=local.y;
        if(!Number.isFinite(xx)||!Number.isFinite(yy)||xx<left||xx>left+width||!panels.some(p=>yy>=p.top&&yy<=p.top+height)){hide();return;}
        const time=model.start+(xx-left)/width*(model.end-model.start);
        const candle=model.candles[Math.min(model.candles.length-1,Math.floor((time-model.start)/CANDLE_MS))],bar=candle[key];
        for(const cursor of cursors){cursor.setAttribute('x1',xx);cursor.setAttribute('x2',xx);cursor.setAttribute('visibility','visible');}
        if(hover)hover.textContent=`${stamp(candle.time_ms)} · ${label} · ${bar?`开 ${value(bar.open)}  高 ${value(bar.high)}  低 ${value(bar.low)}  收 ${value(bar.close)}${candle.forming?' · 形成中':''}${bar.incomplete?' · 含行情缺口':''}`:'无有效报价'}`;
    };
    root.append(svg);
    pointer.draw();
  }
  function quoteRows(payload,now,connectionOk) {
    const s=payload?.available?payload.status:{}, maxAge=Number(payload?.rules?.book_max_age_ms??1500);
    return [['Lighter RH',payload?.symbols?.[0]||'OPENAI'],['Entropy',payload?.symbols?.[1]||'io:OAI']].map(([platform,symbol],i)=>{
      const b=s.books?.[i], buy=quotePrice(b?.ask?.price), sell=quotePrice(b?.bid?.price);
      const time=Number(b?.received_ms), hasTime=Number.isFinite(time)&&time>0;
      const transport=s.market_transport?.[i], receipt=Number(transport?.last_ws_message_ms), age=Number(transport?.last_ws_source_age_ms);
      const delayed=transport?.ws_connected===true&&Number.isFinite(receipt)&&receipt>0&&receipt<=now&&now-receipt<=3000&&Number.isFinite(age)&&age>maxAge;
      const bookFresh=buy!=='—'&&sell!=='—'&&b.connected&&s.running&&hasTime&&time<=now&&now-time<=maxAge&&Number.isFinite(s.observed_ms)&&now>=s.observed_ms&&now-s.observed_ms<10000;
      let state='行情有效';
      if(!connectionOk)state='连接中断 · 旧报价';
      else if(delayed&&!bookFresh)state=`行情延迟 · 约 ${(age/1000).toFixed(1)} 秒`;
      else if(buy==='—'||sell==='—')state='报价缺失';
      else if(!b.connected)state='行情断连 · 旧报价';
      else if(!s.running)state='行情已停止 · 旧报价';
      else if(!hasTime||time>now)state='报价时间异常';
      else if(now-time>maxAge||!Number.isFinite(s.observed_ms)||now<s.observed_ms||now-s.observed_ms>=10000)state='报价过期';
      return {platform,symbol,buy,sell,time:hasTime?stamp(time):'—',state,fresh:state==='行情有效'};
    });
  }
  function renderQuoteCards(quotes, selected, get=id=>document.getElementById(id)) {
    const root=get('comparison-quotes');
    root.replaceChildren();
    const cell=(text,tag='td')=>{const x=document.createElement(tag);x.textContent=text;return x;};
    // Platform positions stay fixed across markets and direction changes.
    // The chart names the trading direction independently of these quote cards.
    for(const q of quotes) {
      const tr=document.createElement('tr'),platform=cell(q.platform),symbol=cell(q.symbol,'small');
      symbol.className='subline';platform.append(symbol);
      const buy=cell(q.buy),sell=cell(q.sell);buy.className=sell.className='quote-price';
      const state=cell(q.state||'');
      state.className=q.fresh?'quote-state':'quote-state quote-stale';
      if(q.time)state.title=`最近盘口：${q.time}`;
      tr.append(platform,buy,sell,state);root.append(tr);
    }
  }
  const visuals={chartModel,renderCharts,renderQuoteCards,quoteRows,displayDirection};
  if(typeof module!=='undefined' && module.exports)module.exports=visuals;
  else window.OpenaiMarketVisuals=visuals;
})();
