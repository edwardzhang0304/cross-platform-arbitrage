use anyhow::{Context, Result, ensure};
use axum::{Router, Json, extract::{State, Path as RoutePath, DefaultBodyLimit}, http::HeaderMap, response::{Html,IntoResponse,Response}, routing::{get, post}};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::{Arc, atomic::{AtomicBool, Ordering}}};
use tokio::sync::Mutex;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};
use crate::{config::AppConfig, openai_inventory::{self as inventory, *}, portable::{Settings, ProfilePaths}, account_profiles::{self,AccountProfile}, secrets};

#[derive(Clone)]
pub struct App { inner: Arc<Mutex<Session>>, token: Arc<String>, lease: Arc<AtomicBool>, root: Arc<PathBuf>, port: u16, paths:ProfilePaths, catalog:Arc<Mutex<()>>, notification_serial:Arc<Mutex<()>>, sessions:Arc<std::sync::RwLock<Vec<ProfileSession>>> }
struct Session { settings: Option<Settings>, password: Option<Zeroizing<String>>, service: Option<InventoryService>, notification:Option<crate::notifications::FeishuSettings>, startup_failure:Option<StartupFailure> }
#[derive(Clone, serde::Serialize)]
struct StartupFailure {
    at_ms:u64,
    operation:&'static str,
    issue:inventory::bootstrap_diagnostics::BootstrapIssue,
}
fn record_bootstrap_result<T>(s:&mut Session,operation:&'static str,result:Result<T>)->Result<T> {
    match result {
        Ok(value)=>{s.startup_failure=None;Ok(value)},
        Err(error)=>{
            let issue=error.downcast_ref::<inventory::bootstrap_diagnostics::BootstrapIssue>().cloned()
                .unwrap_or_else(||inventory::bootstrap_diagnostics::BootstrapIssue {
                    stage:"bootstrap",code:"validation_or_unavailable",message:"账户初始化校验失败".into()});
            s.startup_failure=Some(StartupFailure{at_ms:crate::domain::now_ms(),operation,issue:issue.clone()});
            Err(error.context(issue))
        }
    }
}
#[derive(Clone)]
struct ProfileSession { paths:ProfilePaths, inner:Arc<Mutex<Session>>, token:Arc<String>, lease:Arc<AtomicBool>, notification_serial:Arc<Mutex<()>> }
impl ProfileSession {
    fn new(paths:ProfilePaths,settings:Option<Settings>)->Self {Self {paths,inner:Arc::new(Mutex::new(Session{settings,password:None,service:None,notification:None,startup_failure:None})),
        token:Arc::new(uuid::Uuid::new_v4().to_string()),lease:Arc::new(AtomicBool::new(false)),notification_serial:Arc::new(Mutex::new(()))}}
}
#[derive(Clone)]
struct Host {root:Arc<PathBuf>,port:u16,catalog:Arc<Mutex<()>>,sessions:Arc<std::sync::RwLock<Vec<ProfileSession>>>,token:Arc<String>}
impl Host {
    fn app(&self,id:&str)->Result<App> {
        let slot=self.sessions.read().unwrap().iter().find(|s|s.paths.profile.id==id).cloned().context("账户不存在")?;
        Ok(App{inner:slot.inner,token:slot.token,lease:slot.lease,root:self.root.clone(),port:self.port,paths:slot.paths,
            catalog:self.catalog.clone(),notification_serial:slot.notification_serial,sessions:self.sessions.clone()})
    }
    fn profiles(&self)->Vec<AccountProfile> {self.sessions.read().unwrap().iter().map(|s|s.paths.profile.clone()).collect()}
}
pub fn router(root: PathBuf, port: u16) -> Result<Router> {
    let profiles=account_profiles::load(&root)?;
    let paths=profiles.into_iter().map(|p|ProfilePaths::for_profile(&root,p)).collect::<Vec<_>>();
    let configs=paths.iter().map(ProfilePaths::load).collect::<Result<Vec<_>>>()?;
    for (i,c) in configs.iter().enumerate() {if let Some(c)=c {
        let others=configs.iter().enumerate().filter(|(j,_)|*j!=i).filter_map(|(_,s)|s.as_ref().map(|s|s.strategy.clone())).collect::<Vec<_>>();
        crate::profiles::reject_shared_live_accounts(&c.strategy,&others)?;
    }}
    let sessions=Arc::new(std::sync::RwLock::new(paths.into_iter().zip(configs).map(|(p,c)|ProfileSession::new(p,c)).collect()));
    let host=Host{root:Arc::new(root),port,catalog:Arc::new(Mutex::new(())),sessions,token:Arc::new(uuid::Uuid::new_v4().to_string())};
    let mut router=Router::new();
    // Stable legacy URLs continue to refer to the original accounts only.
    for market in [MarketPair::Openai,MarketPair::Anth] {
        let app=host.app(market.id())?;let profile=app.paths.profile.clone();
        let control_host=host.clone();let monitor_host=host.clone();
        let p=profile.clone();let m=profile.clone();
        let r=Router::new().route(&profile.control_path(),get(move||async move{Html(control_page(&p,&control_host.profiles()))}))
            .route(&profile.monitor_path(),get(move||async move{Html(crate::monitor::account_page(&m,&monitor_host.profiles()))}))
            .route(&profile.inventory_api(),get(status)).route(&profile.control_api(),post(action)).with_state(app);
        router=router.merge(r);
    }
    let host_routes=Router::new().route("/health",get(health)).route("/accounts",get(||async{Html(include_str!("../frontend/accounts.html"))}))
        .route("/api/accounts",get(account_list).post(add_account))
        .route("/accounts/{profile}",get(account_control_page)).route("/accounts/{profile}/monitor",get(account_monitor_page))
        .route("/api/accounts/{profile}/inventory",get(account_status)).route("/api/accounts/{profile}/control",post(account_action))
        .with_state(host);
    Ok(router.merge(host_routes).route("/assets/openai-market-visuals.js",get(||async{([("content-type","application/javascript; charset=utf-8")],include_str!("../frontend/openai-market-visuals.js"))}))
        .route("/assets/inventory-components.js",get(||async{([("content-type","application/javascript; charset=utf-8")],include_str!("../frontend/inventory-components.js"))}))
        .layer(DefaultBodyLimit::max(64*1024))
        .layer(axum::middleware::map_response(|mut r:axum::response::Response|async{
            for (k,v) in [("cache-control","no-store"),("x-frame-options","DENY"),("x-content-type-options","nosniff")] {r.headers_mut().insert(k,v.parse().unwrap());} r
        })))
}
fn control_page(profile:&AccountProfile,profiles:&[AccountProfile])->String {
    let mut html=include_str!("../frontend/portable.html")
        .replace("fetch('/api/openai-inventory'",&format!("fetch('{}'",profile.inventory_api()))
        .replace("/openai-inventory",&profile.monitor_path())
        .replace("/api/portable",&profile.control_api())
        .replace("START_OPENAI_LIVE_STRATEGY",&profile.start_confirmation())
        .replace("CONFIRM_OPENAI_ROLLBACK_RECOVERY",&profile.recovery_confirmation())
        .replace("每平台每组约 15U 名义仓位",&format!("每个平台每组约 {}U 保证金、{}U 持仓金额，亏损保护 {}U",5*profile.capital_multiplier,15*profile.capital_multiplier,30*profile.capital_multiplier))
        .replace("OPENAI 实盘",&format!("{} 实盘",account_profiles::html_escape(&profile.label)))
        .replace("当前 OPENAI 标的",&format!("当前 {} 账户",account_profiles::html_escape(&profile.label)))
        .replace("/*CONTROL_PROFILE*/",&format!("const controlProfile={};",account_profiles::script_json(&profile.descriptor())));
    if profile.market==MarketPair::Anth {html=html.replace("/10000","/100000");}
    html.replace("<main>",&format!("<main>{}",account_profiles::navigation(profile,profiles,false)))
}
async fn account_control_page(State(host):State<Host>,RoutePath(id):RoutePath<String>)->Response {
    match host.app(&id) {Ok(app)=>Html(control_page(&app.paths.profile,&host.profiles())).into_response(),Err(_)=>(axum::http::StatusCode::NOT_FOUND,"账户不存在").into_response()}
}
async fn account_monitor_page(State(host):State<Host>,RoutePath(id):RoutePath<String>)->Response {
    match host.app(&id) {Ok(app)=>Html(crate::monitor::account_page(&app.paths.profile,&host.profiles())).into_response(),Err(_)=>(axum::http::StatusCode::NOT_FOUND,"账户不存在").into_response()}
}
async fn account_status(State(host):State<Host>,RoutePath(id):RoutePath<String>,h:HeaderMap)->Json<Value> {
    match host.app(&id) {Ok(app)=>status(State(app),h).await,Err(_)=>Json(json!({"ok":false,"error":"账户不存在"}))}
}
async fn account_action(State(host):State<Host>,RoutePath(id):RoutePath<String>,h:HeaderMap,r:Json<Request>)->Json<Value> {
    match host.app(&id) {Ok(app)=>action(State(app),h,r).await,Err(_)=>Json(json!({"ok":false,"error":"账户不存在"}))}
}
async fn account_list(State(host):State<Host>,h:HeaderMap)->Json<Value> {
    if local(&h,host.port).is_err(){return Json(json!({"ok":false}));}
    let slots=host.sessions.read().unwrap().clone();let mut profiles=vec![];
    for slot in slots {let s=slot.inner.lock().await;let view=s.service.as_ref().map(InventoryService::status);
        profiles.push(json!({"profile":slot.paths.profile.descriptor(),"configured":s.settings.is_some(),"loaded":s.service.is_some(),"unlocked":s.password.is_some(),
            "status":view.as_ref().map(|v|v.snapshot.status),"held_groups":view.as_ref().map(|v|v.snapshot.lots.len()),"notification_enabled":s.service.as_ref().map(|v|v.notifications.status().enabled)}));}
    Json(json!({"ok":true,"data":{"csrf":host.token.as_str(),"profiles":profiles,"supported_markets":[MarketPair::Openai,MarketPair::Anth],"shared_market_feeds":crate::market_feed::diagnostics().await}}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddAccount {id:String,confirmation:String,market:MarketPair,label:String,
    #[serde(default="inventory::config::default_capital_multiplier")]
    capital_multiplier:u32}
async fn add_account(State(host):State<Host>,h:HeaderMap,Json(r):Json<AddAccount>)->Json<Value> {
    let result=async {
        local(&h,host.port)?;
        ensure!(h.get("x-inventory-token").and_then(|v|v.to_str().ok())==Some(host.token.as_str()),"请刷新账户管理页面后重试");
        ensure!(r.confirmation=="CREATE_ACCOUNT_PROFILE","请确认新增独立账户");
        let mut profile=AccountProfile::new(r.market,r.label,&r.id)?;
        profile.capital_multiplier=r.capital_multiplier;profile.validate()?;
        let _catalog=host.catalog.lock().await;
        let mut profiles=host.profiles();
        if let Some(existing)=profiles.iter().find(|p|p.id==profile.id) {ensure!(existing==&profile,"重复请求的账户信息不一致");return Ok(profile.descriptor());}
        let paths=ProfilePaths::for_profile(&host.root,profile.clone());
        ensure!(!paths.accounts.exists() && !paths.vault.exists() && !paths.database.exists(),"新账户目录已存在数据，拒绝覆盖");
        profiles.push(profile.clone());
        account_profiles::save(&host.root,&profiles)?;
        host.sessions.write().unwrap().push(ProfileSession::new(paths,None));
        Ok::<_,anyhow::Error>(profile.descriptor())
    }.await;
    match result {Ok(profile)=>Json(json!({"ok":true,"data":{"profile":profile}})),Err(e)=>Json(json!({"ok":false,"error":e.to_string()}))}
}
fn local(headers: &HeaderMap, port: u16) -> Result<()> {
    let host = headers.get("host").and_then(|v|v.to_str().ok()).context("缺少本机地址")?;
    ensure!(host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}"), "只允许本机访问");
    if let Some(origin) = headers.get("origin") {
        ensure!(origin.to_str()? == format!("http://{host}"), "拒绝跨站请求");
    }
    Ok(())
}
async fn health(State(host): State<Host>, h: HeaderMap) -> Json<Value> {
    if local(&h, host.port).is_err() { return Json(json!({"ok":false})); }
    Json(json!({"application":"openai-paired-trader","version":env!("CARGO_PKG_VERSION"),"build":crate::build_info::current(),"data_dir":host.root.to_string_lossy(),"sleep_prevention":cfg!(windows),"entropy_info":crate::hyperliquid::info_rate_diagnostics(),
        "profiles":host.profiles().iter().map(AccountProfile::descriptor).collect::<Vec<_>>(),"shared_market_feeds":crate::market_feed::diagnostics().await}))
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
        "vault_exists":app.paths.vault.exists(),"startup_failure":s.startup_failure,
        "notifications":{"saved":notification_path(&app).exists(),"unlocked":s.notification.is_some(),"config":s.notification.as_ref().map(|c|c.public()),"execution_grace_ms":inventory::alerts::EXECUTION_GRACE_MS,"delivery":s.service.as_ref().map(|v|v.notifications.status())},
        "residual_recovery_version":inventory::execution::RESIDUAL_RECOVERY_VERSION,
        "recovery_policy":inventory::execution::recovery_policy(),
        "profile":app.paths.profile.descriptor(),
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
            let text = error.downcast_ref::<inventory::bootstrap_diagnostics::BootstrapIssue>()
                .map(ToString::to_string).unwrap_or_else(||error.to_string());
            let safe = if [request.password.as_str(),request.entropy_key.as_str(),request.lighter_key.as_str(),request.feishu_app_secret.as_str()]
                .iter().any(|secret| !secret.is_empty() && text.contains(secret)) { "操作失败，请检查填写内容".to_string() } else { text };
            Json(json!({"ok":false,"error":safe}))
        }
    }
}
async fn other_strategies(app:&App)->Result<Vec<InventoryConfig>> {
    let mut out=Vec::new();
    let slots=app.sessions.read().unwrap().clone();
    for other in slots.iter().filter(|x|!Arc::ptr_eq(&x.inner,&app.inner)) {
        if let Some(s)=other.paths.load()? {out.push(other.paths.bound_strategy(&s.strategy)?);}
        if let Some(s)=&other.inner.lock().await.settings {out.push(other.paths.bound_strategy(&s.strategy)?);}
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
    if matches!(r.command.as_str(),"pause"|"stop"|"close_all"|"confirm_rollback_recovery") {
        let command=match r.command.as_str(){"pause"=>Control::Pause,"stop"=>Control::Stop,
            "confirm_rollback_recovery"=>{
                ensure!(r.confirmation==app.paths.profile.recovery_confirmation(),
                    "请明确确认当前账户人工恢复；核对通过后仍需单独启动策略");
                Control::ConfirmRollbackRecovery
            },_=>{
            ensure!(r.confirmation=="CLOSE_ALL_POSITIONS","请确认平掉全部持仓");Control::CloseAll}};
        let service=app.inner.lock().await.service.clone().context("账户尚未加载")?;
        service.control(r.id.clone(),command).await?;
        if r.command=="confirm_rollback_recovery" {
            return Ok(json!({"accepted":true,"message":"人工确认已记录；请核对开仓保护已解除、策略仍为已停止，再单独启动实盘策略。"}));
        }
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
            strategy.set_initial_capital(app.paths.profile.capital_multiplier)?;
            if !app.paths.profile.is_legacy() || strategy.market==MarketPair::Anth {strategy.lighter_account=app.paths.profile.alias("lighter");strategy.entropy_account=app.paths.profile.alias("entropy");}
            strategy.entropy_address=r.entropy_address.clone(); strategy.lighter_account_index=Some(index);strategy.lighter_address=Some(r.lighter_address.clone());
            let others=other_strategies(app).await?;
            crate::profiles::reject_shared_live_accounts(&strategy,&others)?;
            let entropy_secret=if app.paths.profile.is_legacy() && strategy.market==MarketPair::Openai {"entropy_trading".into()}else{format!("{}:trading",app.paths.profile.alias("entropy"))};
            let lighter_secret=if app.paths.profile.is_legacy() && strategy.market==MarketPair::Openai {"lighter_trading".into()}else{format!("{}:trading",app.paths.profile.alias("lighter"))};
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
            app.paths.profile.validate_config(&cfg.strategy)?;
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
                let result=inventory::live::read_only_preflight(&bound_strategy,l,e).await;
                let evidence=record_bootstrap_result(&mut s,"preflight",result)?;
                return Ok(json!({"accounts":evidence}));
            }
            ensure!(r.confirmation=="LOAD_EXCLUSIVE_ACCOUNTS", "请先确认原电脑的交易程序已关闭");
            app.paths.upgrade_rules(&mut cfg)?;
            let bound_strategy=app.paths.bound_strategy(&cfg.strategy)?;
            s.settings=Some(cfg.clone());
            let result=inventory::live::bootstrap(&bound_strategy,l,e,false).await;
            let workers=record_bootstrap_result(&mut s,"launch",result)?;
            app.lease.store(true,Ordering::SeqCst);
            let lease=app.lease.clone();let check: Arc<dyn Fn()->bool+Send+Sync>=Arc::new(move||lease.load(Ordering::SeqCst));
            let workers=workers.map(|inner|Box::new(inventory::venue::GuardedBackend{inner,lease:check.clone()}) as Box<dyn inventory::venue::VenueBackend>);
            let result=InventoryService::launch(cfg.strategy,app.paths.database.as_path(),false,Some(workers)).await;
            let result=inventory::bootstrap_diagnostics::checked(result,"ledger_and_public_feed","账本或公共行情初始化失败，请查看诊断包");
            match record_bootstrap_result(&mut s,"launch",result) {Ok(service)=>{service.notifications.set_account_profile(app.paths.profile.clone());service.notifications.configure(s.notification.clone());s.service=Some(service)},Err(error)=>{app.lease.store(false,Ordering::SeqCst);return Err(error);}}
        }
        "start" => {
            ensure!(r.confirmation==app.paths.profile.start_confirmation(),"请明确确认启动实盘策略");
            s.service.as_ref().context("请先加载账户")?.control(r.id.clone(),Control::StartLiveStrategy).await?;
        }
        "lock" | "quit" => {
            let slots=app.sessions.read().unwrap().clone();
            // Validate ALL accounts before stopping any service; one busy account blocks the exit.
            if r.command=="quit" {
                ensure!(r.confirmation=="QUIT_PROGRAM","请确认退出后台程序");
                for other in slots.iter().filter(|x|!Arc::ptr_eq(&x.inner,&app.inner)) {
                    if let Some(service)=&other.inner.lock().await.service {ensure_stopped_for_exit(&service.status().snapshot)?;}
                }
            }
            if let Some(service)=&s.service {ensure_stopped_for_exit(&service.status().snapshot)?;}
            if r.command=="quit" {
                for other in slots.iter().filter(|x|!Arc::ptr_eq(&x.inner,&app.inner)) {
                    let mut session=other.inner.lock().await;
                    if let Some(service)=&session.service {service.control(format!("{}-{}",r.id,other.paths.profile.id),Control::Shutdown).await?;}
                    other.lease.store(false,Ordering::SeqCst);session.service=None;session.password=None;session.notification=None;
                }
            }
            if let Some(service)=&s.service {service.control(r.id.clone(),Control::Shutdown).await?;}
            app.lease.store(false,Ordering::SeqCst);s.service=None;s.password=None;s.notification=None;
            if r.command=="quit" {
                tokio::spawn(async {tokio::time::sleep(std::time::Duration::from_millis(300)).await;std::process::exit(0);});
            }
        }
        _=>anyhow::bail!("未知操作"),
    }
    Ok(json!({"accepted":true}))
}
fn ensure_stopped_for_exit(state:&Snapshot)->Result<()> {
    ensure!(state.pending.is_none() && state.live_orphan.is_none() && state.status==Status::Stopped,
        "请先停止所有已加载账户的策略并等待双腿操作结束，再锁定或退出后台程序");Ok(())
}
fn notification_path(app:&App)->PathBuf {app.paths.vault.with_file_name("feishu.vault")}
async fn notification_action(app:&App,r:&Request)->Result<Value> {
    if r.command=="test_notifications" {
        let cfg=app.inner.lock().await.notification.clone().context("请先解锁并保存飞书通知配置")?;
        crate::notifications::test_profile_message(&cfg,&app.paths.profile).await?;
        return Ok(json!({"accepted":true,"message":"飞书已接受测试消息，请确认手机收到；本次没有下单"}));
    }
    let _serial=app.notification_serial.lock().await;
    let password=app.inner.lock().await.password.clone().context("请先解锁当前账户密钥库")?;
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
        ensure!(r.confirmation=="DISABLE_NOTIFICATIONS","请确认关闭当前账户飞书通知");
        if path.exists(){std::fs::remove_file(path).context("关闭飞书通知失败")?;}
        None
    };
    let mut s=app.inner.lock().await;
    if !s.password.as_ref().is_some_and(|p|p.as_str()==password.as_str()) {
        return Ok(json!({"accepted":true,"message":"通知已保存；当前账户已锁定，请重新解锁启用"}));
    }
    s.notification=setting;
    if let Some(service)=&s.service {service.notifications.configure(s.notification.clone());}
    Ok(json!({"accepted":true,"message":"当前账户通知配置已更新；保存后请发送测试通知，并在手机确认收到"}))
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
        let app=App{inner:Arc::new(Mutex::new(Session{settings:None,password:None,service:None,notification:None,startup_failure:None})),token:Arc::new("secret-test-token".into()),lease:Arc::new(AtomicBool::new(false)),root:Arc::new(PathBuf::new()),port:18794,paths:ProfilePaths::new(std::path::Path::new(""),MarketPair::Openai),catalog:Arc::new(Mutex::new(())),notification_serial:Arc::new(Mutex::new(())),sessions:Arc::new(std::sync::RwLock::new(vec![]))};
        let mut h=HeaderMap::new();h.insert("host","127.0.0.1:18794".parse().unwrap());
        let mut r=Request::default();r.command="launch".into();r.id="test".into();
        assert!(handle(&app,&h,&r).await.is_err());assert!(!app.lease.load(Ordering::SeqCst));
        assert!(app.inner.lock().await.service.is_none());
    }
    #[test]
    fn anth_control_page_and_monitor_target_only_anth() {
        let html=control_page(&AccountProfile::legacy(MarketPair::Anth),&[]);
        assert!(html.contains("fetch('/api/anth-inventory'"));
        assert!(html.contains("fetch('/api/anth-portable'"));
        assert!(!html.contains("fetch('/api/openai-inventory'"));
        assert!(html.contains("START_ANTH_LIVE_STRATEGY"));
        assert!(html.contains("CONFIRM_ANTH_ROLLBACK_RECOVERY"));
        assert!(!html.contains("CONFIRM_OPENAI_ROLLBACK_RECOVERY"));
        assert!(html.contains("/100000"));
        let monitor=crate::monitor::page(MarketPair::Anth,Mode::Live);
        assert!(monitor.contains("ANTH 实盘监控"));assert!(monitor.contains("\"quantityDecimals\":5"));
    }

    #[tokio::test]
    async fn manual_recovery_requires_profile_confirmation_token_and_loaded_service() {
        for market in [MarketPair::Openai,MarketPair::Anth] {
            let app=App{inner:Arc::new(Mutex::new(Session{settings:None,password:None,service:None,notification:None,startup_failure:None})),token:Arc::new("synthetic-token".into()),lease:Arc::new(AtomicBool::new(false)),root:Arc::new(PathBuf::new()),port:18794,paths:ProfilePaths::new(std::path::Path::new(""),market),catalog:Arc::new(Mutex::new(())),notification_serial:Arc::new(Mutex::new(())),sessions:Arc::new(std::sync::RwLock::new(vec![]))};
            let mut h=HeaderMap::new();h.insert("host","127.0.0.1:18794".parse().unwrap());
            let mut r=Request::default();r.command="confirm_rollback_recovery".into();r.id="manual-ack".into();
            assert!(handle(&app,&h,&r).await.unwrap_err().to_string().contains("刷新页面"));
            h.insert("x-inventory-token","synthetic-token".parse().unwrap());
            for wrong in ["", "CONFIRM_OTHER_ROLLBACK_RECOVERY", if market==MarketPair::Openai {"CONFIRM_ANTH_ROLLBACK_RECOVERY"}else{"CONFIRM_OPENAI_ROLLBACK_RECOVERY"}] {
                r.confirmation=wrong.into();
                assert!(handle(&app,&h,&r).await.unwrap_err().to_string().contains("明确确认"));
            }
            r.confirmation=format!("CONFIRM_{}_ROLLBACK_RECOVERY",market.id().to_uppercase());
            assert!(handle(&app,&h,&r).await.unwrap_err().to_string().contains("尚未加载"));
            assert!(!app.lease.load(Ordering::SeqCst));
        }
    }

}

#[cfg(test)]
mod account_host_tests {
    use super::*;
    use rust_decimal::Decimal;
    fn host()->Host {
        let root=Arc::new(std::env::temp_dir().join(format!("cpa-host-{}",uuid::Uuid::new_v4())));
        let sessions=account_profiles::load(&root).unwrap().into_iter().map(|p|ProfileSession::new(ProfilePaths::for_profile(&root,p),None)).collect();
        Host{root,port:18794,catalog:Arc::new(Mutex::new(())),sessions:Arc::new(std::sync::RwLock::new(sessions)),token:Arc::new("synthetic-management".into())}
    }
    fn headers(token:&str)->HeaderMap {let mut h=HeaderMap::new();h.insert("host","127.0.0.1:18794".parse().unwrap());h.insert("x-inventory-token",token.parse().unwrap());h}
    fn request(id:&str,label:&str)->AddAccount {AddAccount{id:id.into(),label:label.into(),market:MarketPair::Openai,confirmation:"CREATE_ACCOUNT_PROFILE".into(),capital_multiplier:1}}
    #[tokio::test]
    async fn unloaded_bootstrap_failure_is_public_scoped_and_cleared_only_on_success() {
        let host=host();let anth=host.app("anth").unwrap();let openai=host.app("openai").unwrap();
        let error=inventory::bootstrap_diagnostics::checked::<()>(
            Err(anyhow::anyhow!("secret-DO-NOT-EXPORT https://private.invalid/token")),
            "entropy_contract","Entropy 保证金模式核验失败").unwrap_err();
        {
            let mut s=anth.inner.lock().await;
            assert!(record_bootstrap_result::<()>(&mut s,"launch",Err(error)).is_err());
            assert!(s.service.is_none());
        }
        let public=status(State(anth.clone()),headers(anth.token.as_str())).await.0;
        assert!(public["data"]["view"].is_null());
        let failure=&public["data"]["startup_failure"];
        assert_eq!(failure["operation"],"launch");assert_eq!(failure["issue"]["stage"],"entropy_contract");
        assert!(failure["at_ms"].as_u64().unwrap()>0);
        assert!(!public.to_string().contains("DO-NOT-EXPORT"));assert!(!public.to_string().contains("private.invalid"));
        assert!(status(State(openai.clone()),headers(openai.token.as_str())).await.0["data"]["startup_failure"].is_null());
        {
            let mut s=anth.inner.lock().await;
            record_bootstrap_result(&mut s,"preflight",Ok(())).unwrap();
        }
        assert!(status(State(anth.clone()),headers(anth.token.as_str())).await.0["data"]["startup_failure"].is_null());
        assert!(!host.root.exists());
    }
    #[tokio::test]
    async fn create_is_authenticated_atomic_idempotent_and_survives_host_restart() {
        let host=host();let id=uuid::Uuid::new_v4().to_string();
        for token in ["",host.app("openai").unwrap().token.as_str()] {
            assert_eq!(add_account(State(host.clone()),headers(token),Json(request(&id,"second"))).await.0["ok"],false);assert!(!host.root.exists());
        }
        let h=headers(&host.token);let result=add_account(State(host.clone()),h.clone(),Json(request(&id,"second"))).await.0;
        assert_eq!(result["ok"],true);let pid=result["data"]["profile"]["id"].as_str().unwrap();
        assert_eq!(host.profiles().len(),3);assert!(host.app(pid).unwrap().paths.load().unwrap().is_none());
        assert_eq!(add_account(State(host.clone()),h.clone(),Json(request(&id,"second"))).await.0,result);
        assert_eq!(add_account(State(host.clone()),h.clone(),Json(request(&id,"different"))).await.0["ok"],false);
        let mut changed=request(&id,"second");changed.capital_multiplier=10;
        assert_eq!(add_account(State(host.clone()),h.clone(),Json(changed)).await.0["ok"],false);
        let mut unapproved=request(&uuid::Uuid::new_v4().to_string(),"unapproved");unapproved.capital_multiplier=100;
        assert_eq!(add_account(State(host.clone()),h.clone(),Json(unapproved)).await.0["ok"],false);
        assert_eq!(add_account(State(host.clone()),h.clone(),Json(request(&uuid::Uuid::new_v4().to_string(),"second"))).await.0["ok"],false);
        let mut cross=h;cross.insert("origin","https://evil.example".parse().unwrap());
        assert_eq!(account_list(State(host.clone()),cross.clone()).await.0["ok"],false);
        assert_eq!(add_account(State(host.clone()),cross,Json(request(&uuid::Uuid::new_v4().to_string(),"third"))).await.0["ok"],false);
        assert_eq!(account_profiles::load(&host.root).unwrap(),host.profiles());
        assert!(router(host.root.as_ref().clone(),18794).is_ok());
        assert!(host.app("../openai").is_err());std::fs::remove_dir_all(host.root.as_ref()).unwrap();
    }
    #[tokio::test]
    async fn same_market_controls_tokens_vaults_confirmations_and_views_are_account_scoped() {
        let host=host();let id=uuid::Uuid::new_v4().to_string();
        let result=add_account(State(host.clone()),headers(&host.token),Json(request(&id,"second"))).await.0;
        let first=host.app("openai").unwrap();let second=host.app(result["data"]["profile"]["id"].as_str().unwrap()).unwrap();
        assert!(!Arc::ptr_eq(&first.inner,&second.inner));assert!(!Arc::ptr_eq(&first.lease,&second.lease));assert_ne!(notification_path(&first),notification_path(&second));
        first.inner.lock().await.password=Some(Zeroizing::new("synthetic-private-password".into()));
        for (target,source) in [(&first,&second),(&second,&first)] {
            for command in ["start","stop","close_all","launch","lock","quit","save_credentials","save_notifications","confirm_rollback_recovery"] {
                let mut r=Request::default();r.command=command.into();r.id="test".into();
                assert!(handle(target,&headers(&source.token),&r).await.unwrap_err().to_string().contains("刷新页面"));
            }
            let mut r=Request::default();r.command="start".into();r.id="test".into();r.confirmation=source.paths.profile.start_confirmation();
            assert!(handle(target,&headers(&target.token),&r).await.unwrap_err().to_string().contains("明确确认"));
        }
        assert_eq!(status(State(second.clone()),headers("")).await.0["data"]["vault_unlocked"],false);
        assert_eq!(status(State(first.clone()),headers("")).await.0["data"]["vault_unlocked"],true);
        let page=control_page(&second.paths.profile,&host.profiles());assert!(!page.contains("fetch('/api/portable'"));assert!(page.contains(&second.paths.profile.start_confirmation()));
        let monitor=crate::monitor::account_page(&second.paths.profile,&host.profiles());assert!(monitor.contains(&second.paths.profile.inventory_api()));assert!(monitor.contains("\"id\":"));
        assert!(!page.contains("synthetic-private-password"));std::fs::remove_dir_all(host.root.as_ref()).unwrap();
    }
    #[tokio::test]
    async fn additional_profiles_save_lock_and_unlock_their_own_encrypted_credentials_without_network() {
        let host=host();
        for (i,market) in [MarketPair::Openai,MarketPair::Anth].into_iter().enumerate() {
            let mut add=request(&uuid::Uuid::new_v4().to_string(),&format!("synthetic account {i}"));add.market=market;
            if market==MarketPair::Openai {add.capital_multiplier=10;}
            let result=add_account(State(host.clone()),headers(&host.token),Json(add)).await.0;
            let app=host.app(result["data"]["profile"]["id"].as_str().unwrap()).unwrap();
            let mut r=Request::default();r.command="save_credentials".into();r.id="synthetic-save".into();r.password=format!("synthetic-vault-password-{i}");
            r.entropy_address=format!("0x{:040x}",500+i);r.entropy_key=format!("{:064x}",1000+i);
            r.lighter_address=format!("0x{:040x}",600+i);r.lighter_account_index=(700+i).to_string();r.lighter_key_index="4".into();r.lighter_key="1".repeat(80);
            handle(&app,&headers(&app.token),&r).await.unwrap();
            let saved=app.paths.load().unwrap().unwrap();assert_eq!(saved.strategy.market,market);
            let scale=if market==MarketPair::Openai {10}else{1};
            assert_eq!(saved.strategy.capital_multiplier,scale);
            assert_eq!(saved.strategy.group_notional,Decimal::from(15*scale));
            assert_eq!(saved.strategy.max_loss_usdc,Decimal::from(30*scale));
            assert!(control_page(&app.paths.profile,&host.profiles()).contains(&format!("每个平台每组约 {}U 保证金、{}U 持仓金额，亏损保护 {}U",5*scale,15*scale,30*scale)));
            let mut wrong=saved.clone();wrong.strategy.set_initial_capital(if scale==10 {1}else{10}).unwrap();
            assert!(app.paths.save(&wrong).is_err());assert_eq!(app.paths.load().unwrap().unwrap().strategy,saved.strategy);
            assert_eq!(saved.strategy.lighter_account,app.paths.profile.alias("lighter"));
            let vault=std::fs::read_to_string(&app.paths.vault).unwrap();assert!(!vault.contains(&r.entropy_key));assert!(!vault.contains(&r.lighter_key));
            r.command="lock".into();r.id="synthetic-lock".into();handle(&app,&headers(&app.token),&r).await.unwrap();assert!(app.inner.lock().await.password.is_none());
            r.command="unlock".into();r.id="synthetic-unlock".into();handle(&app,&headers(&app.token),&r).await.unwrap();
            assert!(app.inner.lock().await.password.is_some());assert!(app.inner.lock().await.service.is_none());assert!(!app.paths.database.exists());
        }
        assert!(host.app("openai").unwrap().inner.lock().await.password.is_none());assert!(!host.app("openai").unwrap().paths.vault.exists());
        assert_eq!(account_profiles::load(&host.root).unwrap(),host.profiles());
        std::fs::remove_dir_all(host.root.as_ref()).unwrap();
    }
    #[test]
    fn whole_program_exit_requires_all_profiles_stopped_and_reconciled() {
        let c=serde_json::from_str(include_str!("../tests/fixtures/inventory/live-strategy.json")).unwrap();
        let mut state=Snapshot::new(c).unwrap();state.status=Status::Stopped;assert!(ensure_stopped_for_exit(&state).is_ok());
        for status in [Status::Running,Status::NeedsAttention,Status::Recovering] {state.status=status;assert!(ensure_stopped_for_exit(&state).is_err());}
    }
}
