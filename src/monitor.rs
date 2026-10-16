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
    // Account headings and profit descriptions belong to the shared components.
    if paper { html=html.replace("正在连接实盘策略…","正在连接模拟策略…")
        .replace("data-openai-mode=\"live\"","data-openai-mode=\"paper\""); }
    html
}

/// Account ID binds a same-market monitor to exactly one private ledger.
pub fn account_page(profile:&crate::account_profiles::AccountProfile,profiles:&[crate::account_profiles::AccountProfile])->String {
    let market=profile.market;
    let descriptor=serde_json::json!({"id":profile.id,"label":profile.label,"market":market,"mode":"live",
        "quantityDecimals":market.quantity_decimals(),"api":profile.inventory_api(),"symbols":[market.lighter_symbol(),market.entropy_symbol()]});
    include_str!("../frontend/openai-live-monitor.html")
        .replace("/*PROFILE_CONFIG*/",&format!("window.InventoryProfile={};",crate::account_profiles::script_json(&descriptor)))
        .replace("OPENAI 实盘监控",&format!("{} 实盘监控",crate::account_profiles::html_escape(&profile.label)))
        .replace("<nav><a href=\"/\">控制台</a><a href=\"/openai-inventory\">OPENAI</a><a href=\"/anth-inventory\">ANTH</a></nav>",&crate::account_profiles::navigation(profile,profiles,true))
}
