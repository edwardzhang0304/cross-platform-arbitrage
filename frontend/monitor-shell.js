/* Read-only monitor chrome. Account actions remain in the control center. */
(function(root){
  'use strict';
  const $=id=>document.getElementById(id),ui=root.GlassUI;
  let navigation=null,current=null,lastMarket=null;
  function mount(profile,list){
    current=profile;
    root.OpenaiMarketVisuals.renderQuoteCards(root.OpenaiMarketVisuals.quoteRows({symbols:profile.symbols,available:false},Date.now(),false),'shortL',$);
    $('monitor-account').textContent=profile.label||`${profile.market.toUpperCase()} ${profile.mode==='paper'?'模拟':'实盘'}监控`;
    if(profile.mode==='paper'){document.querySelector('h1').textContent='模拟监控';$('account-management-link').hidden=true;$('monitor-brand').href='/';}
    $('control-link').href=profile.mode==='paper'?'/':profile.control_path||(profile.market==='anth'?'/anth':'/');
    if(profile.mode==='live'){
      navigation=ui.createAccountNavigation($('account-tabs'),profile.id||profile.market,true);
      if(list.length)navigation.render(list.map(p=>({profile:p,loaded:false,status:null})));
      navigation.refresh();
    }else $('account-tabs').hidden=true;
  }
  function render(payload){
    const v=payload.view,s=v.snapshot,st=ui.state(s),pair=ui.paired(v,Date.now());
    $('monitor-error').hidden=true;document.body.classList.remove('stale');
    $('monitor-state').textContent=st.text;$('monitor-state').className='badge '+st.tone;
    $('monitor-pair-status').textContent=current.mode==='paper'?'模拟账户 · 虚拟资金':pair.text;
    $('monitor-pair-status').className=pair.ok?'good':'muted';
    const warning=s.status==='needs_attention'||s.entry_margin_block||v.transient_warning||v.order_lookup_note;
    $('monitor-warning').hidden=!warning;
    $('monitor-warning-title').textContent=s.entry_margin_block?'保证金保护生效，开仓与加仓已暂停':s.status==='needs_attention'?'账户需要处理，请进入控制中心查看恢复入口':'有待核对的信息';
    $('monitor-reason').textContent=[s.reason,v.transient_warning,v.order_lookup_note].filter(Boolean).join('\n');
    lastMarket={symbols:current.symbols,available:true,status:{running:true,observed_ms:Date.now(),books:(v.books||[]).map(b=>({bid:b.bids?.[0]?{price:b.bids[0].price}:null,ask:b.asks?.[0]?{price:b.asks[0].price}:null,connected:b.connected,received_ms:b.received_ms}))},rules:{book_max_age_ms:s.config.book_max_age_ms}};
    navigation?.refresh();
  }
  function disconnected(message){
    const unloaded=message==='策略尚未启动';
    document.body.classList.add('stale');$('monitor-error').hidden=false;
    $('monitor-error').textContent=unloaded?'当前账户尚未加载，请到控制中心完成账户检查与加载。':'无法读取当前账户状态；下方保留的行情和表格可能已过期，请查看控制中心。';
    $('monitor-state').textContent=unloaded?'未加载':'连接中断';$('monitor-state').className='badge warn';
    $('monitor-pair-status').textContent='暂不能确认配平';$('monitor-pair-status').className='muted';
    $('updated').textContent='等待重新连接';
    if(lastMarket)root.OpenaiMarketVisuals.renderQuoteCards(root.OpenaiMarketVisuals.quoteRows(lastMarket,Date.now(),false),'shortL',$);
    navigation?.refresh();
  }
  root.MonitorUI={mount,render,disconnected};
})(window);
