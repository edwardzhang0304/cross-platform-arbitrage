//! Credential-free local control plane. No real-account or vault routes are registered.
use crate::{monitor, openai_inventory::{*, execution::RESIDUAL_RECOVERY_VERSION}};
use axum::{extract::{State, DefaultBodyLimit}, http::HeaderMap, response::Html, routing::{get, post}, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone)]
pub struct PaperApp { pub services: Arc<BTreeMap<MarketPair, InventoryService>>, pub port:u16, pub tokens:Arc<BTreeMap<MarketPair,String>> }
pub const PAPER_MARKETS:[MarketPair;2]=[MarketPair::Openai,MarketPair::Anth];
pub fn router(app: PaperApp) -> Router {
    Router::new()
        .route("/",get(||async{Html(include_str!("../frontend/paper.html"))}))
        .route("/anth-inventory",get(||async{Html(monitor::page(MarketPair::Anth,Mode::Paper))}))
        .route("/openai-inventory",get(||async{Html(monitor::page(MarketPair::Openai,Mode::Paper))}))
        .route("/assets/openai-market-visuals.js",get(||async{([("content-type","application/javascript; charset=utf-8")],include_str!("../frontend/openai-market-visuals.js"))}))
        .route("/assets/inventory-components.js",get(||async{([("content-type","application/javascript; charset=utf-8")],include_str!("../frontend/inventory-components.js"))}))
        .route("/api/anth-inventory",get(|s:State<PaperApp>,h:HeaderMap|status(s,h,MarketPair::Anth)))
        .route("/api/openai-inventory",get(|s:State<PaperApp>,h:HeaderMap|status(s,h,MarketPair::Openai)))
        .route("/api/anth-paper",post(|s:State<PaperApp>,h:HeaderMap,r:Json<PaperCommand>|control(s,h,r,MarketPair::Anth)))
        .route("/api/openai-paper",post(|s:State<PaperApp>,h:HeaderMap,r:Json<PaperCommand>|control(s,h,r,MarketPair::Openai)))
        .route("/health",get(||async{Json(json!({"application":"paired-paper","mode":"paper","live_orders":false,"markets":PAPER_MARKETS,"pid":std::process::id(),"build":crate::build_info::current()}))}))
        .layer(DefaultBodyLimit::max(2048))
        .layer(axum::middleware::map_response(|mut r:axum::response::Response|async {
            for (k,v) in [("cache-control","no-store"),("x-frame-options","DENY"),("x-content-type-options","nosniff")] {
                r.headers_mut().insert(k,v.parse().unwrap());
            } r
        })).with_state(app)
}
fn local(app:&PaperApp,h:&HeaderMap)->bool {
    let host=h.get("host").and_then(|v|v.to_str().ok()).unwrap_or_default();
    (host==format!("127.0.0.1:{}",app.port)||host==format!("localhost:{}",app.port))
        && h.get("origin").is_none_or(|v|v.to_str().ok()==Some(&format!("http://{host}")))
}
async fn status(State(app):State<PaperApp>,h:HeaderMap,market:MarketPair)->Json<Value> {
    if !local(&app,&h) {return Json(json!({"ok":false}));}
    let view=app.services.get(&market).map(InventoryService::status);
    let rules=view.as_ref().map(|v|crate::build_info::rules_fingerprint(&v.snapshot.config));
    Json(json!({"ok":true,"data":{"view":view,"build":crate::build_info::current(),"rules_fingerprint":rules,
        "profile":{"market":market,"mode":"paper"},"csrf":app.tokens.get(&market),"paper_build":true,
        "live_build":false,"process_dry_run":true,"residual_recovery_version":RESIDUAL_RECOVERY_VERSION,
        "recovery_policy":execution::recovery_policy()}}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaperCommand { market:MarketPair, mode:Mode, command:String, id:String }
fn authorize(app:&PaperApp,h:&HeaderMap,r:&PaperCommand,market:MarketPair)->anyhow::Result<()> {
    anyhow::ensure!(local(app,h) && app.tokens.get(&market).is_some_and(|token|
        h.get("x-inventory-token").and_then(|v|v.to_str().ok())==Some(token.as_str())),"请从本机对应标的模拟控制台操作");
    anyhow::ensure!(r.market==market && r.mode==Mode::Paper,"标的或运行模式不匹配");
    anyhow::ensure!(!r.id.is_empty() && r.id.len()<=128,"缺少操作编号");
    Ok(())
}
async fn control(State(app):State<PaperApp>,h:HeaderMap,Json(r):Json<PaperCommand>,market:MarketPair)->Json<Value> {
    let result=async {
        authorize(&app,&h,&r,market)?;
        let service=app.services.get(&market).ok_or_else(||anyhow::anyhow!("该模拟策略尚未加载"))?;
        let command=match r.command.as_str() {"start"=>Control::Start,"pause"=>Control::Pause,"stop"=>Control::Stop,"close_all"=>Control::CloseAll,_=>anyhow::bail!("模拟版不支持此操作")};
        service.control(r.id,command).await
    }.await;
    match result {Ok(())=>Json(json!({"ok":true})),Err(e)=>Json(json!({"ok":false,"error":e.to_string()}))}
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn two_paper_profiles_cannot_control_each_other_or_accept_live_commands() {
        let app=PaperApp {services:Arc::new(BTreeMap::new()),port:18794,
            tokens:Arc::new([(MarketPair::Openai,"openai-test".into()),(MarketPair::Anth,"anth-test".into())].into())};
        for market in PAPER_MARKETS {
            let other=if market==MarketPair::Openai {MarketPair::Anth}else{MarketPair::Openai};
            let mut headers=HeaderMap::new();headers.insert("host","127.0.0.1:18794".parse().unwrap());
            headers.insert("x-inventory-token",app.tokens[&market].parse().unwrap());
            let mut r=PaperCommand{market,mode:Mode::Paper,command:"stop".into(),id:"scoped-stop".into()};
            assert!(authorize(&app,&headers,&r,market).is_ok());
            assert!(authorize(&app,&headers,&r,other).is_err());
            headers.insert("x-inventory-token",app.tokens[&other].parse().unwrap());
            assert!(authorize(&app,&headers,&r,market).is_err());
            headers.insert("x-inventory-token",app.tokens[&market].parse().unwrap());
            r.market=other;assert!(authorize(&app,&headers,&r,market).is_err());
            r.market=market;r.mode=Mode::Live;assert!(authorize(&app,&headers,&r,market).is_err());
        }
    }
}
