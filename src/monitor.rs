use crate::openai_inventory::{MarketPair, Mode};

/// Only trusted enum values are substituted; no wallet/configuration text enters HTML.
pub fn page(market: MarketPair, mode: Mode) -> String {
    let paper = mode == Mode::Paper;
    let descriptor = serde_json::json!({"market":market,"mode":mode,
        "quantityDecimals":market.quantity_decimals(),"api":format!("/api/{}-inventory",market.id()),
        "symbols":[market.lighter_symbol(),market.entropy_symbol()]});
    let mut html = include_str!("../frontend/openai-live-monitor.html")
        .replace("/*PROFILE_CONFIG*/", &format!("window.InventoryProfile={descriptor};"))
        .replace("OPENAI 实盘监控", &format!("{} {}监控",market.label(),if paper {"模拟"} else {"实盘"}));
    if paper { html=html.replace("两平台真实账户","两平台虚拟账户").replace("正在连接实盘策略…","正在连接模拟策略…")
        .replace("data-openai-mode=\"live\"","data-openai-mode=\"paper\"")
        .replace("已完成双腿平仓部分的实际收益", "已完成双腿模拟平仓部分的收益")
        .replace("对应已结算资金费", "对应资金费模拟估算"); }
    html
}
