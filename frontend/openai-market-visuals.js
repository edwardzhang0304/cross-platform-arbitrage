/* Read-only live chart and quotes. No strategy controls or polling. */
(function () {
  'use strict';
  const stamp = t => t ? new Date(t).toLocaleString('zh-CN', {hour12:false}) : '—';
  const quotePrice = n => n == null || !Number.isFinite(Number(n)) || Number(n)<=0 ? '—' : Number(n).toLocaleString('en-US',{minimumFractionDigits:2,maximumFractionDigits:4});
  function chartModel(raw) {
    const price=v=>v==null||!Number.isFinite(Number(v))||Number(v)<=0?null:Number(v);
    let rows=(Array.isArray(raw)?raw:[]).map(p=>{
      let lb=price(p.lighter_bid),la=price(p.lighter_ask),eb=price(p.entropy_bid),ea=price(p.entropy_ask);
      if(lb!=null&&la!=null&&lb>la){lb=null;la=null;}
      if(eb!=null&&ea!=null&&eb>ea){eb=null;ea=null;}
      return {time_ms:Number(p.time_ms),lighter:lb!=null&&la!=null?(lb+la)/2:null,
        entropy:eb!=null&&ea!=null?(eb+ea)/2:null,
        shortL:lb!=null&&ea!=null?lb-ea:null,shortE:eb!=null&&la!=null?eb-la:null,
        closeL:la!=null&&eb!=null?la-eb:null,closeE:ea!=null&&lb!=null?ea-lb:null};
    }).filter(p=>Number.isFinite(p.time_ms)&&p.time_ms>0).sort((a,b)=>a.time_ms-b.time_ms).slice(-14401);
    if(!rows.some(p=>p.lighter!=null||p.entropy!=null))return null;
    const start=Math.max(rows[0].time_ms,rows.at(-1).time_ms-14400000),end=Math.max(rows.at(-1).time_ms,start+300000);
    rows=rows.filter(p=>p.time_ms>=start);
    const range=(keys,zero)=>{
      const values=rows.flatMap(p=>keys.map(k=>p[k]).filter(v=>v!=null));
      if(!values.length)return {min:-1,max:1};
      let min=Math.min(...values),max=Math.max(...values);
      if(zero){min=Math.min(min,0);max=Math.max(max,0);}
      const pad=Math.max((max-min)*.12,.05);return {min:min-pad,max:max+pad};
    };
    return {rows,start,end,spread:range(['shortL','shortE','closeL','closeE'],true),prices:range(['lighter','entropy'],false)};
  }
  function displayDirection(payload) {
    const b=payload?.status?.books||[],n=x=>Number(x);
    const values=[b[0]?.bid?.price,b[0]?.ask?.price,b[1]?.bid?.price,b[1]?.ask?.price];
    if(values.every(x=>x!=null&&Number.isFinite(n(x))&&n(x)>0)&&n(values[0])<=n(values[1])&&n(values[2])<=n(values[3]))
      return n(values[0])-n(values[3])>=n(values[2])-n(values[1])?'shortL':'shortE';
    const row=chartModel(payload?.chart_points)?.rows.slice().reverse().find(r=>r.shortL!=null&&r.shortE!=null);
    return !row||row.shortL>=row.shortE?'shortL':'shortE';
  }
  const chartRanges=new Map();
  let chartRangeSession=null;
  const chartPointers=new WeakMap();
  function pointerState(root) {
    let state=chartPointers.get(root);
    if(!state){
      state={point:null,draw:null};
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
    root.replaceChildren();status.textContent='';if(hover)hover.textContent='';
    const model=chartModel(payload?.chart_points);
    if(!model)return;
    const node=(tag,attrs={},text)=>{const n=document.createElementNS('http://www.w3.org/2000/svg',tag);
      for(const [k,v] of Object.entries(attrs))n.setAttribute(k,String(v));if(text!=null)n.textContent=text;return n;};
    const compact=root.clientWidth>0&&root.clientWidth<580;
    const svg=node('svg',{viewBox:`0 0 ${compact?480:1000} 255`,role:'img','aria-label':'当前方向的 Bid−Ask 与 Ask−Bid 价差',style:'user-select:none;-webkit-user-select:none'});
    svg.append(node('title',{},'绿线 Bid−Ask；红线 Ask−Bid'));
    const left=compact?62:74,width=compact?390:890,height=154,x=t=>left+(t-model.start)/(model.end-model.start)*width;
    const selected=displayDirection(payload);
    const closing=selected==='shortL'?'closeL':'closeE';
    const visibleValues=model.rows.flatMap(p=>[p[selected],p[closing]]).filter(v=>v!=null);
    if(chartRangeSession!==payload?.status?.started_ms){chartRanges.clear();chartRangeSession=payload?.status?.started_ms;}
    if(visibleValues.length){
      const lo=Math.min(0,...visibleValues),hi=Math.max(0,...visibleValues);
      const previous=chartRanges.get(selected);
      if(previous&&lo>=previous.min&&hi<=previous.max)model.spread=previous;
      else {const step=2;model.spread={min:Math.floor((Math.min(lo,previous?.min??lo)-.25)/step)*step,max:Math.ceil((Math.max(hi,previous?.max??hi)+.25)/step)*step};chartRanges.set(selected,model.spread);}
    }
    const latest=model.rows.at(-1),value=v=>v==null?'—':v.toFixed(4);
    svg.append(node('text',{x:left,y:25,fill:'#526277','font-size':12},selected==='shortL'?'空 Lighter / 多 Entropy':'空 Entropy / 多 Lighter'));
    const panels=[{top:62,range:model.spread,title:'盘口价差（第一档）· U / 份',
      series:[[selected,'Bid−Ask','#139b55'],[closing,'Ask−Bid','#e24444']]}];
    for(const panel of panels){
      const {top,range}=panel,y=v=>top+height-(v-range.min)/(range.max-range.min)*height;
      
      panel.series.forEach(([key,label,color],i)=>svg.append(node('text',{x:left+i*(compact?200:440),y:top-15,fill:color,style:`fill:${color}`,'font-size':13},`${label}  ${value(latest[key])}`)));
      for(let i=0;i<=3;i++){
        const p=range.min+(range.max-range.min)*i/3,yy=y(p);
        svg.append(node('line',{x1:left,x2:left+width,y1:yy,y2:yy,stroke:'#e2e8f0'}),
          node('text',{x:left-8,y:yy+4,'text-anchor':'end',fill:'#526277','font-size':12},p.toFixed(2)));
      }
      if(range.min<0&&range.max>0)svg.append(node('line',{x1:left,x2:left+width,y1:y(0),y2:y(0),stroke:'#64748b','stroke-dasharray':'4 4'}));
      for(let i=0;i<=4;i++){
        const t=model.start+(model.end-model.start)*i/4,xx=x(t);
        svg.append(node('line',{x1:xx,x2:xx,y1:top,y2:top+height,stroke:'#f1f5f9'}),
          node('text',{x:xx,y:top+height+22,'text-anchor':i===0?'start':i===4?'end':'middle',fill:'#526277','font-size':12},new Date(t).toLocaleTimeString('zh-CN',{hour:'2-digit',minute:'2-digit',hour12:false})));
      }
      for(const [key,label,color] of panel.series){
        let path='',previous=null;
        for(const p of model.rows){
          if(p[key]==null||p.time_ms<model.start){previous=null;continue;}
          const command=previous!=null&&p.time_ms-previous<=2500?'L':'M';
          path+=`${command}${x(p.time_ms).toFixed(2)},${y(p[key]).toFixed(2)} `;previous=p.time_ms;
        }
        const line=node('path',{d:path.trim(),fill:'none',stroke:color,'stroke-width':1.5,'data-series':key});
        line.append(node('title',{},label));svg.append(line);
        if(latest[key]!=null)svg.append(node('circle',{cx:x(latest.time_ms),cy:y(latest[key]),r:2.5,fill:color}));
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
        let lo=0,hi=model.rows.length-1;
        while(lo<hi){const mid=(lo+hi)>>1;if(model.rows[mid].time_ms<time)lo=mid+1;else hi=mid;}
        if(lo>0&&Math.abs(model.rows[lo-1].time_ms-time)<Math.abs(model.rows[lo].time_ms-time))lo--;
        const p=model.rows[lo];
        for(const cursor of cursors){cursor.setAttribute('x1',xx);cursor.setAttribute('x2',xx);cursor.setAttribute('visibility','visible');}
        if(hover)hover.textContent=`${stamp(p.time_ms)} · Bid−Ask ${value(p[selected])} · Ask−Bid ${value(p[closing])}`;
    };
    root.append(svg);
    pointer.draw();
  }
  function quoteRows(payload,now,connectionOk) {
    const s=payload?.available?payload.status:{}, maxAge=Number(payload?.rules?.book_max_age_ms??1500);
    return [['Lighter RH',payload?.symbols?.[0]||'OPENAI'],['Entropy',payload?.symbols?.[1]||'io:OAI']].map(([platform,symbol],i)=>{
      const b=s.books?.[i], buy=quotePrice(b?.ask?.price), sell=quotePrice(b?.bid?.price);
      const time=Number(b?.received_ms), hasTime=Number.isFinite(time)&&time>0;
      let state='行情有效';
      if(buy==='—'||sell==='—')state='报价缺失';
      else if(!connectionOk)state='连接中断 · 旧报价';
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
      const state=cell('');
      tr.append(platform,buy,sell,state);root.append(tr);
    }
  }
  const visuals={chartModel,renderCharts,renderQuoteCards,quoteRows,displayDirection};
  if(typeof module!=='undefined' && module.exports)module.exports=visuals;
  else window.OpenaiMarketVisuals=visuals;
})();
