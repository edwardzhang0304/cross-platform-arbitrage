/* Shared, read-only presentation primitives for control and monitoring pages. */
(function(root){
  'use strict';
  const labels={running:'运行中',stopped:'已停止',warming:'行情预热中',needs_attention:'需要处理',recovering:'正在核对',recovering_exposure:'正在配平',paused:'已暂停',paused_entries:'仅平仓和止损',closing:'正在平仓'};
  const number=v=>v===null||v===undefined||v===''?NaN:Number(v);
  const amount=v=>Number.isFinite(number(v))?number(v).toLocaleString('zh-CN',{minimumFractionDigits:2,maximumFractionDigits:2})+' U':'—';
  const escape=v=>String(v??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
  function entryAvailable(a){const m=a?.entry_margin;if(!m)return {text:amount(a?.free_margin),detail:''};const known=m.isolated_only&&Number.isFinite(number(m.collateral))&&Number.isFinite(number(m.reported_available));return {text:known?amount(Math.max(0,Math.min(number(m.collateral),number(m.reported_available)))):'待核对',detail:`平台显示可用 ${amount(a.free_margin)} · 未划入逐仓 ${amount(m.collateral)}`};}
  function state(s){if(!s)return {text:'未加载',tone:'neutral'};if(s.emergency_exit?.flat_confirmed_ms&&!s.emergency_exit.completed_ms)return {text:'已清仓，账本核对中',tone:'warn'};return {text:labels[s.status]||s.status||'未知状态',tone:s.status==='running'?'good':['stopped','paused'].includes(s.status)?'neutral':s.status==='needs_attention'?'bad':'warn'};}
  function paired(view,now){const s=view?.snapshot,a=view?.accounts||[];if(!s||a.length!==2)return {text:'加载账户后核对两边持仓',ok:false};const by=v=>a.find(a=>a.venue===v),l=by('lighter'),e=by('entropy');const max=s.config?.account_max_age_ms??3000;const fresh=r=>r&&r.authenticated&&number(r.observed_ms)<=now+1000&&now-number(r.observed_ms)<=max;const ledger=s.positions||[];
    if(!fresh(l)||!fresh(e))return {text:'账户数据待更新，暂不能确认配平',ok:false};
    if(s.pending||s.orphan||s.orphan_exposure||s.emergency_exit&&!s.emergency_exit.completed_ms)return {text:'订单或持仓仍在核对，以实盘监控为准',ok:false};
    if(!Number.isFinite(number(l.position_units))||!Number.isFinite(number(e.position_units)))return {text:'持仓数据不完整，暂不能确认配平',ok:false};
    if(number(l.position_units)+number(e.position_units)!==0||number(ledger[0]?.units)!==number(l.position_units)||number(ledger[1]?.units)!==number(e.position_units))return {text:'持仓与配对账本待核对',ok:false};
    return {text:'双腿数量与账本一致',ok:true};
  }
  function createAccountNavigation(element,selectedId,monitor=false){
    let key='',last=0,busy=false;
    function render(items){const next=JSON.stringify(items.map(i=>[i.profile.id,i.profile.label,i.loaded,i.status]));if(next===key)return;key=next;element.replaceChildren();for(const item of items){const p=item.profile,a=document.createElement('a'),st=state(item.loaded?{status:item.status}:null);a.className='account-tab';a.href=monitor?p.monitor_path:p.control_path;if(p.id===selectedId)a.setAttribute('aria-current','page');a.innerHTML=`<span class="status-dot ${st.tone}"></span><span><strong>${escape(p.label)}</strong><small>${escape(st.text)}</small></span>`;element.append(a);}const selected=element.querySelector('[aria-current="page"]');if(selected)element.scrollLeft=selected.offsetLeft-element.offsetLeft-(element.clientWidth-selected.offsetWidth)/2;}
    async function refresh(){if(busy||Date.now()-last<5000)return;busy=true;last=Date.now();try{const r=await(await fetch('/api/accounts',{cache:'no-store'})).json();if(r.ok&&Array.isArray(r.data?.profiles))render(r.data.profiles);}catch{/* Current account remains independently readable. */}finally{busy=false;}}
    return {render,refresh};
  }
  const api={number,amount,escape,entryAvailable,state,paired,createAccountNavigation};
  if(typeof module!=='undefined'&&module.exports)module.exports=api;
  root.GlassUI=api;
})(typeof window==='undefined'?globalThis:window);
