use anyhow::Result;
use clap::Parser;
use openai_paired_trader::{openai_inventory::{InventoryService, MarketPair, Mode, Control}, paper_server::{self, PaperApp}, profiles};
use std::{path::PathBuf, collections::BTreeMap, sync::Arc};

#[derive(Parser)]
#[command(about="公开行情、虚拟资金；不加载密钥，也不能发送实盘订单")]
struct Args {
    /// Must be a new empty directory or this simulator's own data directory.
    #[arg(long)] data_dir:PathBuf,
    #[arg(long,default_value_t=18794)] port:u16,
    /// Also run a separate OPENAI simulation; never connects to Windows accounts.
    #[arg(long)] include_openai:bool,
    #[arg(long)] start:bool,
}
#[tokio::main]
async fn main()->Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let args=Args::parse();
    let (root,_lock)=profiles::prepare_paper_root(&args.data_dir)?;
    let listener=tokio::net::TcpListener::bind(("127.0.0.1",args.port)).await?;
    let mut markets=vec![MarketPair::Anth];
    if args.include_openai {markets.push(MarketPair::Openai);}
    let mut services=BTreeMap::new();
    for market in markets {
        let config=profiles::load_paper_config(&root,market)?;
        let id=profiles::ProfileId::new(market,Mode::Paper);
        let service=InventoryService::launch(config,&id.ledger(&root),true,None).await?;
        if args.start && service.status().snapshot.pending.is_none() {
            service.control(uuid::Uuid::new_v4().to_string(),Control::Start).await?;
        }
        services.insert(market,service);
    }
    let app=PaperApp {services:Arc::new(services),port:args.port,token:Arc::new(uuid::Uuid::new_v4().to_string())};
    tracing::info!("ANTH 模拟监控：http://127.0.0.1:{}/anth-inventory；无实盘下单能力",args.port);
    axum::serve(listener,paper_server::router(app)).with_graceful_shutdown(async{let _=tokio::signal::ctrl_c().await;}).await?;
    Ok(())
}
