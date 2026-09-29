use anyhow::{Context, Result, ensure};
use axum::{Router, Json, extract::{State, DefaultBodyLimit}, http::HeaderMap, response::Html, routing::{get, post}};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::{Arc, atomic::{AtomicBool, Ordering}}};
use tokio::sync::Mutex;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};
use crate::{config::AppConfig, openai_inventory::{self as inventory, *}, portable::{Settings, ProfilePaths}, secrets};

#[derive(Clone)]
pub struct App { inner: Arc<Mutex<Session>>, token: Arc<String>, lease: Arc<AtomicBool>, root: Arc<PathBuf>, port: u16, paths:ProfilePaths, catalog:Arc<Mutex<()>>, notification_serial:Arc<Mutex<()>>, sessions:Arc<Vec<Arc<Mutex<Session>>>> }
struct Session { settings: Option<Settings>, password: Option<Zeroizing<String>>, service: Option<InventoryService>, notification:Option<crate::notifications::FeishuSettings> }

pub fn router(root: PathBuf, port: u16) -> Result<Router> {
    let pairs=[MarketPair::Openai,MarketPair::Anth];
    let paths=pairs.map(|m|ProfilePaths::new(&root,m));
    let configs=paths.iter().map(ProfilePaths::load).collect::<Result<Vec<_>>>()?;
    let strategies:Vec<_>=configs.iter().flatten().map(|s|s.strategy.clone()).collect();
    for c in &strategies {crate::profiles::reject_shared_live_accounts(c,&strategies)?;}
    let sessions=Arc::new(configs.into_iter().map(|settings|Arc::new(Mutex::new(Session{settings,password:None,service:None,notification:None}))).collect::<Vec<_>>());
    let catalog=Arc::new(Mutex::new(()));
    let mut router=Router::new();
    for (i,paths) in paths.into_iter().enumerate() {
        let market=paths.market;
        let app = App {inner:sessions[i].clone(),token:Arc::new(uuid::Uuid::new_v4().to_string()),
            lease:Arc::new(AtomicBool::new(false)),root:Arc::new(root.clone()),port,paths,catalog:catalog.clone(),notification_serial:Arc::new(Mutex::new(())),sessions:sessions.clone()};
        let base=if market==MarketPair::Openai {"/"}else{"/anth"};
        let api=format!("/api/{}-inventory",market.id());
        let control=if market==MarketPair::Openai {"/api/portable"}else{"/api/anth-portable"};
        let app_router=Router::new().route(base,get(move||async move{Html(control_page(market))}))
            .route(&format!("/{}-inventory",market.id()),get(move||async move{Html(crate::monitor::page(market,Mode::Live))}))
            .route(&api,get(status)).route(control,post(action));
        let app_router=if i==0 {app_router.route("/health",get(health))}else{app_router};
        router=router.merge(app_router.with_state(app));
    }
    Ok(router.route("/assets/openai-market-visuals.js",get(||async{([("content-type","application/javascript; charset=utf-8")],include_str!("../frontend/openai-market-visuals.js"))}))
        .route("/assets/inventory-components.js",get(||async{([("content-type","application/javascript; charset=utf-8")],include_str!("../frontend/inventory-components.js"))}))
        .layer(DefaultBodyLimit::max(64*1024))
        .layer(axum::middleware::map_response(|mut r:axum::response::Response|async{
            for (k,v) in [("cache-control","no-store"),("x-frame-options","DENY"),("x-content-type-options","nosniff")] {r.headers_mut().insert(k,v.parse().unwrap());} r
        })))
}
fn control_page(market:MarketPair)->String {
    let nav="<nav style=\"display:flex;gap:24px;margin-bottom:18px\"><a href=\"/\">OPENAI 实盘配置</a><a href=\"/anth\">ANTH 实盘配置</a></nav>";
    let html=include_str!("../frontend/portable.html");
    let html=if market==MarketPair::Openai {html.to_owned()}else{
        html.replace("OPENAI", "ANTH").replace("/openai-inventory","/anth-inventory")
            .replace("/api/portable","/api/anth-portable").replace("/10000","/100000")
    };
    html.replace("<main>",&format!("<main>{nav}"))
}
fn local(headers: &HeaderMap, port: u16) -> Result<()> {
    let host = headers.get("host").and_then(|v|v.to_str().ok()).context("缺少本机地址")?;
    ensure!(host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}"), "只允许本机访问");
    if let Some(origin) = headers.get("origin") {
        ensure!(origin.to_str()? == format!("http://{host}"), "拒绝跨站请求");
    }
    Ok(())
}
async fn health(State(app): State<App>, h: HeaderMap) -> Json<Value> {
    if local(&h, app.port).is_err() { return Json(json!({"ok":false})); }
    Json(json!({"application":"openai-paired-trader","version":env!("CARGO_PKG_VERSION"),"build":crate::build_info::current(),"data_dir":app.root.to_string_lossy(),"sleep_prevention":cfg!(windows)}))
}
async fn status(State(app): State<App>, h: HeaderMap) -> Json<Value> {
    if local(&h, app.port).is_err() { return Json(json!({"ok":false,"error":"只允许本机访问"})); }
    let s = app.inner.lock().await;
    let view = s.service.as_ref().map(InventoryService::status);
    let rules = view.as_ref().map(|v| &v.snapshot.config).or(s.settings.as_ref().map(|x| &x.strategy)).map(crate::build_info::rules_fingerprint);
    Json(json!({"ok":true,"data":{
        "view":view,"csrf":app.token.as_str(),
        "build":crate::build_info::current(),"rules_fingerprint":rules,
        "live_build":cfg!(feature="openai-inventory-live"),"process_dry_run":false,"kill_switch":false,
        "vault_unlocked":s.password.is_some(),"configured":s.settings.is_some(),
        "vault_exists":app.paths.vault.exists(),
        "notifications":{"saved":notification_path(&app).exists(),"unlocked":s.notification.is_some(),"config":s.notification.as_ref().map(|c|c.public()),"execution_grace_ms":inventory::alerts::EXECUTION_GRACE_MS,"delivery":s.service.as_ref().map(|v|v.notifications.status())},
        "residual_recovery_version":inventory::execution::RESIDUAL_RECOVERY_VERSION,
        "profile":{"market":app.paths.market,"mode":"live"},
        "strategy":s.settings.as_ref().map(|x|&x.strategy),"sleep_prevention":cfg!(windows),
        "data_dir":app.root.to_string_lossy()
    }}))
}
#[derive(Default, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
pub struct Request {
    #[serde(default)] command: String,
    #[serde(default)] id: String,
    #[serde(default)] confirmation: String,
    #[serde(default)] password: String,
    #[serde(default)] entropy_address: String,
    #[serde(default)] entropy_key: String,
    #[serde(default)] lighter_address: String,
    #[serde(default)] lighter_account_index: String,
    #[serde(default)] lighter_key_index: String,
    #[serde(default)] lighter_key: String,
    #[serde(default)] feishu_app_id:String,
    #[serde(default)] feishu_app_secret:String,
    #[serde(default)] feishu_receive_id_type:String,
    #[serde(default)] feishu_receive_id:String,
}
async fn action(State(app): State<App>, h: HeaderMap, Json(request): Json<Request>) -> Json<Value> {
    let result = handle(&app, &h, &request).await;
    match result {
        Ok(value) => Json(json!({"ok":true,"data":value})),
        // Do not include chained HTTP errors, response bodies or user-entered secrets.
        Err(error) => {
            let text = error.to_string();
            let safe = if [request.password.as_str(),request.entropy_key.as_str(),request.lighter_key.as_str(),request.feishu_app_secret.as_str()]
                .iter().any(|secret| !secret.is_empty() && text.contains(secret)) { "操作失败，请检查填写内容".to_string() } else { text };
            Json(json!({"ok":false,"error":safe}))
        }
    }
}
async fn other_strategies(app:&App)->Result<Vec<InventoryConfig>> {
    let mut out=Vec::new();
    for market in [MarketPair::Openai,MarketPair::Anth].into_iter().filter(|m|*m!=app.paths.market) {
        let paths=ProfilePaths::new(&app.root,market);
        if let Some(s)=paths.load()? {out.push(paths.bound_strategy(&s.strategy)?);}
    }
    for other in app.sessions.iter().filter(|x|!Arc::ptr_eq(x,&app.inner)) {
        if let Some(s)=&other.lock().await.settings {
            let paths=ProfilePaths::new(&app.root,s.strategy.market);
            out.push(paths.bound_strategy(&s.strategy)?);
        }
    }
    Ok(out)
}
async fn handle(app: &App, h: &HeaderMap, r: &Request) -> Result<Value> {
    local(h, app.port)?;
    ensure!(h.get("x-inventory-token").and_then(|v|v.to_str().ok()) == Some(app.token.as_str()), "请刷新页面后重试");
    ensure!(!r.id.is_empty() && r.id.len() <=128, "缺少操作编号");
    // Notification HTTP and defensive controls cannot wait for another
    // profile's bootstrap or account preflight while it holds the catalog.
    if matches!(r.command.as_str(),"save_notifications"|"test_notifications"|"disable_notifications") {
        return notification_action(app,r).await;
    }
    if matches!(r.command.as_str(),"pause"|"stop"|"close_all") {
        let command=match r.command.as_str(){"pause"=>Control::Pause,"stop"=>Control::Stop,_=>{
            ensure!(r.confirmation=="CLOSE_ALL_POSITIONS","请确认平掉全部持仓");Control::CloseAll}};
        let service=app.inner.lock().await.service.clone().context("账户尚未加载")?;
        service.control(r.id.clone(),command).await?;
        return Ok(json!({"accepted":true}));
    }
    let _catalog=app.catalog.lock().await;
    let mut s = app.inner.lock().await;
    match r.command.as_str() {
        "save_credentials" => {
            ensure!(s.service.is_none() && !app.paths.database.exists(), "已有持仓账本或运行中的程序，请使用原配置迁移，不要重新开户配置");
            ensure!(r.password.len() >= 12, "密钥库密码至少 12 位");
            let index: i64 = r.lighter_account_index.parse().context("Lighter 账户 INDEX 必须是数字")?;
            let key_index: u8 = r.lighter_key_index.parse().context("API KEY INDEX 必须是数字")?;
            let address_ok = |v: &str| v.len()==42 && v.starts_with("0x") && v[2..].bytes().all(|b|b.is_ascii_hexdigit()) && v[2..].bytes().any(|b|b!=b'0');
            ensure!(address_ok(&r.entropy_address) && address_ok(&r.lighter_address), "请填写完整公开钱包地址");
            secrets::private_key_address(&r.entropy_key).context("Entropy API 私钥格式错误")?;
            crate::lighter_runtime::LighterApiCredential::from_hex(index,key_index,&r.lighter_key).context("Lighter API 私钥格式错误")?;
            let mut strategy: InventoryConfig = serde_json::from_str(include_str!("../config/strategy.example.json"))?;
            strategy.market=app.paths.market;
            if strategy.market==MarketPair::Anth {strategy.lighter_account="anth:live:lighter".into();strategy.entropy_account="anth:live:entropy".into();}
            strategy.entropy_address=r.entropy_address.clone(); strategy.lighter_account_index=Some(index);strategy.lighter_address=Some(r.lighter_address.clone());
            let others=other_strategies(app).await?;
            crate::profiles::reject_shared_live_accounts(&strategy,&others)?;
            let entropy_secret=if strategy.market==MarketPair::Anth {"anth:live:entropy:trading"}else{"entropy_trading"};
            let lighter_secret=if strategy.market==MarketPair::Anth {"anth:live:lighter:trading"}else{"lighter_trading"};
            let mut accounts=AppConfig::default(); accounts.secrets.vault_path=app.paths.vault.to_string_lossy().into_owned();accounts.secrets.allow_env_fallback=false;
            accounts.accounts=vec![serde_json::from_value(json!({"account_id":strategy.entropy_account,"address":strategy.entropy_address,"secret_id":entropy_secret}))?];
            let settings=Settings{accounts,strategy}; settings.strategy.validate()?; settings.strategy.validate_live_identity()?;
            app.paths.check_public_address(&settings.strategy,&r.lighter_address)?;
            let path=app.paths.vault.as_path();
            secrets::upsert_secret(path,&r.password,secrets::SecretUpsert { secret_id:entropy_secret.into(),account_id:settings.strategy.entropy_account.clone(),address:r.entropy_address.clone(),api_wallet_private_key:r.entropy_key.clone() }).context("Entropy 加密保存失败，请检查密码")?;
            secrets::upsert_lighter_secret(path,&r.password,secrets::LighterSecretUpsert {secret_id:lighter_secret.into(),account_id:settings.strategy.lighter_account.clone(),l1_address:r.lighter_address.clone(),account_index:index,api_key_index:key_index,api_private_key:r.lighter_key.clone()}).context("Lighter 加密保存失败，请检查密码")?;
            app.paths.bind_public_address(&settings.strategy,&r.lighter_address)?;
            app.paths.save(&settings)?;s.settings=Some(settings);s.password=Some(Zeroizing::new(r.password.clone()));
        }
        "unlock" => {
            ensure!(s.service.is_none(),"账户已加载，无需重复解锁");
            let summary=secrets::unlock_vault(app.paths.vault.as_path(),&r.password).context("解锁失败，请检查密钥库文件和密码")?;
            let settings=app.paths.load()?.context("缺少该标的账户或策略配置，请检查数据目录")?;
            let (_,entry)=inventory::account_binding::resolve(&settings.strategy,&settings.accounts.accounts,&summary.entries)?;
            app.paths.bind_public_address(&settings.strategy,&entry.address)?;
            s.settings=Some(settings);
            s.password=Some(Zeroizing::new(r.password.clone()));
            s.notification=secrets::load_notification_settings(&notification_path(app),&r.password).ok();
        }
        "preflight" | "launch" => {
            ensure!(s.service.is_none(), "账户已加载，请查看当前状态");
            let mut cfg=s.settings.clone().context("请先配置账户或导入迁移数据")?;
            crate::profiles::ProfileId::new(app.paths.market,Mode::Live).validate(&cfg.strategy)?;
            ensure!(app.paths.load()?.is_some_and(|disk|serde_json::to_value(disk).ok()==serde_json::to_value(&cfg).ok()),"配置文件已改变；请重新加载并核对绑定");
            let bound_strategy=app.paths.bound_strategy(&cfg.strategy)?;
            let others=other_strategies(app).await?;
            crate::profiles::reject_shared_live_accounts(&bound_strategy,&others)?;
            let password=s.password.as_ref().context("请先解锁密钥库")?;
            let summary=secrets::unlock_vault(app.paths.vault.as_path(),password).context("密钥库校验失败")?;
            let (account,entry)=inventory::account_binding::resolve(&cfg.strategy,&cfg.accounts.accounts,&summary.entries)?;
            app.paths.bind_public_address(&cfg.strategy,&entry.address)?;
            let l=secrets::load_lighter_secret_by_id(app.paths.vault.as_path(),password,&entry.secret_id,Some(&cfg.strategy.lighter_account)).context("Lighter 凭据与配置不一致")?;
            let e=secrets::load_account_secret(&cfg.accounts,account,Some(password)).context("Entropy 凭据与配置不一致")?;
            if r.command=="preflight" {
                let evidence=inventory::live::read_only_preflight(&bound_strategy,l,e).await.context("账户检查未通过，请核对网络、API 授权、账户及逐仓 3 倍设置")?;
                return Ok(json!({"accounts":evidence}));
            }
            ensure!(r.confirmation=="LOAD_EXCLUSIVE_ACCOUNTS", "请先确认原电脑的交易程序已关闭");
            app.paths.upgrade_rules(&mut cfg)?;
            let bound_strategy=app.paths.bound_strategy(&cfg.strategy)?;
            s.settings=Some(cfg.clone());
            let workers=inventory::live::bootstrap(&bound_strategy,l,e,false).await.context("账户加载失败，请先运行只读检查")?;
            app.lease.store(true,Ordering::SeqCst);
            let lease=app.lease.clone();let check: Arc<dyn Fn()->bool+Send+Sync>=Arc::new(move||lease.load(Ordering::SeqCst));
            let workers=workers.map(|inner|Box::new(inventory::venue::GuardedBackend{inner,lease:check.clone()}) as Box<dyn inventory::venue::VenueBackend>);
            let result=InventoryService::launch(cfg.strategy,app.paths.database.as_path(),false,Some(workers)).await;
            match result {Ok(service)=>{service.notifications.configure(s.notification.clone());s.service=Some(service)},Err(error)=>{app.lease.store(false,Ordering::SeqCst);return Err(error.context("账本加载失败，请检查迁移文件是否完整"));}}
        }
        "start" => {
            ensure!(r.confirmation==if app.paths.market==MarketPair::Openai {inventory::config::LIVE_STRATEGY_CONFIRMATION}else{"START_ANTH_LIVE_STRATEGY"},"请明确确认启动实盘策略");
            s.service.as_ref().context("请先加载账户")?.control(r.id.clone(),Control::StartLiveStrategy).await?;
        }
        "lock" | "quit" => {
            if r.command=="quit" {
                ensure!(r.confirmation=="QUIT_PROGRAM","请确认退出后台程序");
                for other in app.sessions.iter().filter(|x|!Arc::ptr_eq(x,&app.inner)) {
                    let mut other=other.lock().await;
                    if let Some(service)=&other.service {
                        let state=service.status().snapshot;
                        ensure!(state.pending.is_none()&&state.live_orphan.is_none()&&state.status==Status::Stopped,
                            "另一标的仍在运行；请分别停止两个策略并等待双腿操作结束");
                        service.control(format!("{}-other",r.id),Control::Shutdown).await?;
                        other.service=None;other.password=None;other.notification=None;
                    }
                }
            }
            if let Some(service)=&s.service {
                let state=service.status().snapshot;
                ensure!(state.pending.is_none()&&state.live_orphan.is_none()&&state.status==Status::Stopped,
                    "请先停止交易并等待未完成的双腿处理结束，再锁定或退出");
                service.control(r.id.clone(),Control::Shutdown).await?;
            }
            app.lease.store(false,Ordering::SeqCst);s.service=None;s.password=None;s.notification=None;
            if r.command=="quit" {
                tokio::spawn(async {tokio::time::sleep(std::time::Duration::from_millis(300)).await;std::process::exit(0);});
            }
        }
        _=>anyhow::bail!("未知操作"),
    }
    Ok(json!({"accepted":true}))
}
fn notification_path(app:&App)->PathBuf {app.paths.vault.with_file_name("feishu.vault")}
async fn notification_action(app:&App,r:&Request)->Result<Value> {
    if r.command=="test_notifications" {
        let cfg=app.inner.lock().await.notification.clone().context("请先解锁并保存飞书通知配置")?;
        crate::notifications::test_message(&cfg,app.paths.market).await?;
        return Ok(json!({"accepted":true,"message":"飞书已接受测试消息，请确认手机收到；本次没有下单"}));
    }
    let _serial=app.notification_serial.lock().await;
    let password=app.inner.lock().await.password.clone().context("请先解锁本标的密钥库")?;
    let path=notification_path(app);
    let setting=if r.command=="save_notifications" {
        let cfg=crate::notifications::FeishuSettings{app_id:r.feishu_app_id.trim().into(),app_secret:r.feishu_app_secret.trim().into(),receive_id_type:r.feishu_receive_id_type.clone(),receive_id:r.feishu_receive_id.trim().into()};
        cfg.validate()?;
        let save_password=password.clone();
        // Argon2 and disk sync run off the actor/runtime and without session
        // locks, so saving a notification key cannot delay a stop command.
        Some(tokio::task::spawn_blocking(move|| {
            secrets::save_notification_settings(&path,&save_password,&cfg).context("飞书配置加密保存失败")?;
            Ok::<_,anyhow::Error>(cfg)
        }).await.context("飞书配置保存任务失败")??)
    } else {
        ensure!(r.confirmation=="DISABLE_NOTIFICATIONS","请确认关闭本标的飞书通知");
        if path.exists(){std::fs::remove_file(path).context("关闭飞书通知失败")?;}
        None
    };
    let mut s=app.inner.lock().await;
    if !s.password.as_ref().is_some_and(|p|p.as_str()==password.as_str()) {
        return Ok(json!({"accepted":true,"message":"通知已保存；本标的已锁定，请重新解锁启用"}));
    }
    s.notification=setting;
    if let Some(service)=&s.service {service.notifications.configure(s.notification.clone());}
    Ok(json!({"accepted":true,"message":"本标的通知配置已更新；保存后请发送测试通知，并在手机确认收到"}))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn localhost_and_origin_are_strict() {
        let mut h=HeaderMap::new();h.insert("host","127.0.0.1:18794".parse().unwrap());
        assert!(local(&h,18794).is_ok());
        h.insert("origin","https://evil.example".parse().unwrap());assert!(local(&h,18794).is_err());
        h.remove("origin");h.insert("host","127.0.0.1.evil.example:18794".parse().unwrap());assert!(local(&h,18794).is_err());
    }
    #[tokio::test]
    async fn control_without_session_token_cannot_create_files_or_launch() {
        let app=App{inner:Arc::new(Mutex::new(Session{settings:None,password:None,service:None,notification:None})),token:Arc::new("secret-test-token".into()),lease:Arc::new(AtomicBool::new(false)),root:Arc::new(PathBuf::new()),port:18794,paths:ProfilePaths::new(std::path::Path::new(""),MarketPair::Openai),catalog:Arc::new(Mutex::new(())),notification_serial:Arc::new(Mutex::new(())),sessions:Arc::new(vec![])};
        let mut h=HeaderMap::new();h.insert("host","127.0.0.1:18794".parse().unwrap());
        let mut r=Request::default();r.command="launch".into();r.id="test".into();
        assert!(handle(&app,&h,&r).await.is_err());assert!(!app.lease.load(Ordering::SeqCst));
        assert!(app.inner.lock().await.service.is_none());
    }
    #[test]
    fn anth_control_page_and_monitor_target_only_anth() {
        let html=control_page(MarketPair::Anth);
        assert!(html.contains("fetch('/api/anth-inventory'"));
        assert!(html.contains("fetch('/api/anth-portable'"));
        assert!(!html.contains("fetch('/api/openai-inventory'"));
        assert!(html.contains("START_ANTH_LIVE_STRATEGY"));
        assert!(html.contains("/100000"));
        let monitor=crate::monitor::page(MarketPair::Anth,Mode::Live);
        assert!(monitor.contains("ANTH 实盘监控"));assert!(monitor.contains("\"quantityDecimals\":5"));
    }

}
