/* Shared read-only inventory components. Market precision is presentation metadata. */
(function () {
  'use strict';
  const escapeHtml = value => String(value ?? '').replace(/[&<>"']/g,
    c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
  const number = value => value == null || value === '' ? NaN : Number(value);
  const money = value => Number.isFinite(number(value)) ? number(value).toFixed(2) : '—';
  const amount = value => Number.isFinite(number(value)) ? `${money(value)} U` : '—';
  const time = value => value ? new Date(value).toLocaleString('zh-CN', {hour12:false}) : '—';
  const venueName = venue => ({lighter:'Lighter RH', entropy:'Entropy'}[venue] || '—');
  function profit(value) {
    const raw = number(value), rounded = Number.isFinite(raw) ? Number(raw.toFixed(2)) : NaN;
    return {text:Number.isFinite(rounded) ? `${rounded.toFixed(2)} U` : '—',
      className:rounded > 0 ? 'good' : rounded < 0 ? 'badtext' : '',
      title:Number.isFinite(raw) ? `${raw.toFixed(6)} U` : ''};
  }

  function create(profile, get = id => document.getElementById(id)) {
    const paper = profile.mode === 'paper';
    const quantity = value => Number.isFinite(number(value))
      ? (number(value) / (10 ** profile.quantityDecimals)).toFixed(profile.quantityDecimals) : '—';
    // One column definition supplies the heading, width, alignment and value.
    // Empty and populated tables therefore use exactly the same geometry.
    const tables = [
      {id:'accounts', title:paper ? '两平台虚拟账户' : '两平台真实账户', empty:'账户数据暂不可用', minWidth:720,
        columns:[
          {key:'venue', label:'平台', width:24, value:r => venueName(r.venue), className:'account-name'},
          {key:'equity', label:'权益', width:19, numeric:true, value:r => amount(r.equity)},
          {key:'free_margin', label:'可用资金', width:19, numeric:true, value:r => amount(r.free_margin)},
          {key:'position_units', label:'持仓数量', width:19, numeric:true, value:r => quantity(r.position_units)},
          {key:'margin', label:'保证金模式', width:19, align:'center', value:r => `${r.isolated ? '逐仓' : '非逐仓'} ${r.leverage}×`}
        ]},
      {id:'lots', title:'持有的配对仓位', empty:'暂无持仓', minWidth:1000,
        columns:[
          {key:'opened_ms', label:'开仓时间', width:22, value:r => time(r.opened_ms)},
          {key:'direction', label:'方向', width:24, value:r => r.direction === 'lighter_short' ? '空 Lighter / 多 Entropy' : '空 Entropy / 多 Lighter'},
          {key:'units', label:'每腿数量', width:12, numeric:true, value:r => quantity(r.units)},
          {key:'entry_spread', label:'开仓价差', width:12, numeric:true, value:r => amount(r.entry_spread)},
          {key:'remaining_funding', label:'资金费净收支', width:15, numeric:true, value:r => profit(r.profit?.remaining_funding),
            title:'分摊给剩余持仓的已结算资金费：收入为正，支出为负'},
          {key:'estimated_exit_net', label:'预计净利润', width:15, numeric:true, value:r => profit(r.profit?.estimated_exit_net),
            title:'剩余持仓按当前盘口平仓，扣除开平仓成本并计入已结算资金费的估算'}
        ]},
      {id:'fills', title:'最近成交', empty:'暂无成交', minWidth:840,
        columns:[
          {key:'time_ms', label:'时间', width:26, value:r => time(r.time_ms)},
          {key:'venue', label:'平台', width:18, value:r => venueName(r.venue)},
          {key:'side', label:'买 / 卖', width:12, value:r => r.side === 'buy' ? '买入' : '卖出'},
          {key:'units', label:'数量', width:16, numeric:true, value:r => quantity(r.units)},
          {key:'price', label:'成交价', width:16, numeric:true, value:r => money(r.price)},
          {key:'fee', label:'手续费', width:12, numeric:true, value:r => amount(r.fee)}
        ]}
    ];
    const metrics = [{id:'groups', label:'持仓/平仓组数'}, {id:'pnl', label:'净收益'}, {id:'closed-pnl', label:'平仓净收益'}];
    const emptyRow = table => `<tr><td colspan="${table.columns.length}" class="empty">${table.empty}</td></tr>`;
    function mount() {
      get('status-metrics').innerHTML = metrics.map(m =>
        `<div class="metric" data-metric="${m.id}"><small>${m.label}</small><strong id="${m.id}">—</strong></div>`).join('')+'<p id="emergency-progress" role="status" hidden style="grid-column:1 / -1;margin:8px 0 0"></p>';
      get('inventory-tables').innerHTML = tables.map(t => `<section class="card" data-module="${t.id}" aria-labelledby="${t.id}-heading">
        <h2 id="${t.id}-heading">${t.title}</h2><div class="scroll"><table class="inventory-table" style="--table-min-width:${t.minWidth}px" aria-labelledby="${t.id}-heading">
        <colgroup>${t.columns.map(c => `<col style="width:${c.width}%">`).join('')}</colgroup>
        <thead><tr>${t.columns.map(c => `<th scope="col" data-field="${c.key}" class="${c.numeric ? 'numeric' : c.align || ''}"${c.title ? ` title="${escapeHtml(c.title)}"` : ''}>${c.label}</th>`).join('')}</tr></thead>
        <tbody id="${t.id}">${emptyRow(t)}</tbody></table></div></section>`).join('');
    }
    function renderTable(table, rows) {
      get(table.id).innerHTML = rows.length ? rows.map(row => `<tr>${table.columns.map(column => {
        const raw = column.value(row), cell = raw != null && typeof raw === 'object' ? raw : {text:raw ?? '—'};
        const classes = [column.numeric ? 'numeric' : column.align, column.className, cell.className].filter(Boolean).join(' ');
        return `<td data-field="${column.key}" data-label="${escapeHtml(column.label)}"${classes ? ` class="${classes}"` : ''} title="${escapeHtml(cell.title || cell.text)}">${escapeHtml(cell.text)}</td>`;
      }).join('')}</tr>`).join('') : emptyRow(table);
    }
    function render(view) {
      const snapshot = view.snapshot;
      const emergency=snapshot.emergency_exit, progress=get('emergency-progress');
      progress.hidden=!emergency || (emergency.completed_ms && snapshot.status!=='stopped');
      progress.textContent=!emergency?'':emergency.completed_ms?'紧急全部平仓完成：两平台持仓为零，已停止交易。':
        `紧急平仓 · 最大滑点 5% · 剩余 Lighter ${quantity(emergency.remaining_units?.[0])} / Entropy ${quantity(emergency.remaining_units?.[1])}。`+
        (emergency.flat_confirmed_ms?'持仓已归零，仍在核对原订单和成交记录。':'正在按各平台实际持仓独立减仓。')+
        (emergency.warnings||[]).filter(Boolean).join('；')+(emergency.accounting_error||'');
      get('groups').textContent = `${(snapshot.lots || []).length} / ${snapshot.closed_groups || 0}`;
      for (const [id, value] of [['pnl', view.net_pnl], ['closed-pnl', view.profit_accounting?.closed_net_profit]]) {
        const cell = profit(value);
        get(id).textContent = cell.text;
        get(id).className = cell.className;
      }
      get('pnl').title = '累计已实现盈亏＋剩余仓位平仓估算－交易手续费＋已结算资金费净收支；包含滑点预留，未结算资金费不计入';
      get('closed-pnl').title = `${paper ? '已完成双腿模拟平仓部分的收益' : '已完成双腿平仓部分的实际收益'}－对应开平仓手续费＋${paper ? '对应资金费模拟估算' : '对应已结算资金费'}；不含未平仓浮动盈亏`;
      const accounts = ['lighter', 'entropy'].map(venue => (view.accounts || []).find(a => a.venue === venue)).filter(Boolean);
      const profits = new Map((view.profit_accounting?.lots || []).map(p => [p.lot_id, p]));
      const lots = (snapshot.lots || []).map(lot => ({...lot, direction:snapshot.direction, profit:profits.get(lot.id)}));
      const fills = Object.values(snapshot.fills || {}).sort((a, b) => number(b.time_ms) - number(a.time_ms)).slice(0, 20);
      [accounts, lots, fills].forEach((rows, i) => renderTable(tables[i], rows));
    }
    return {mount, render};
  }
  const components = {create};
  if (typeof module !== 'undefined' && module.exports) module.exports = components;
  else window.InventoryComponents = components;
})();
