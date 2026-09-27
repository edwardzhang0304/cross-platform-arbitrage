#![cfg_attr(windows, windows_subsystem = "windows")]
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use openai_paired_trader::{portable, server};

#[derive(Parser)]
#[command(version, about="OPENAI 双平台实盘：双击启动，浏览器配置，后台运行")]
struct Cli {
    #[arg(long)] data_dir: Option<PathBuf>,
    #[arg(long, default_value_t=18794)] port: u16,
    #[arg(long)] no_browser: bool,
    #[command(subcommand)] command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Only copies account configuration and consistent ledger; never reads keys.
    ExportLegacy { #[arg(long)] source: PathBuf, #[arg(long)] account_config: PathBuf, #[arg(long)] output: PathBuf },
}
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        let message = format!("启动失败：{error}\n请确认没有另一个程序占用 18794 端口，并检查 data 目录权限。");
        #[cfg(windows)] unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::*;
            let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
            let title: Vec<u16> = "OPENAI 双平台交易\0".encode_utf16().collect();
            MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONERROR);
        }
        #[cfg(not(windows))] eprintln!("{message}");
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Command::ExportLegacy {source, account_config, output}) = cli.command {
        return portable::export_legacy(&source, &account_config, &output);
    }
    let exe = std::env::current_exe()?;
    let data = cli.data_dir.unwrap_or_else(|| exe.parent().unwrap().join("data"));
    std::fs::create_dir_all(&data)?;
    let root = data.canonicalize()?;
    let url = format!("http://127.0.0.1:{}", cli.port);
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(2)).build()?;
    if let Ok(response) = client.get(format!("{url}/health")).send().await {
        let v: serde_json::Value = response.json().await.context("端口已被其他程序占用")?;
        ensure!(v["application"] == "openai-paired-trader" && v["data_dir"] == root.to_string_lossy().as_ref(), "端口已被其他程序或不同数据目录占用");
        if !cli.no_browser { open_browser(&url)?; }
        return Ok(());
    }
    let (root, _lock) = portable::prepare(&root)?;
    std::env::set_current_dir(&root)?;
    let _awake = openai_paired_trader::power::Awake::acquire()?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, cli.port)).await?;
    let app = server::router(root, cli.port);
    // A missing default browser must not tear down the background service.
    // The same local address remains available for manual navigation.
    if !cli.no_browser { let _ = open_browser(&url); }
    axum::serve(listener, app).await?;
    Ok(())
}
fn open_browser(url: &str) -> Result<()> {
    #[cfg(windows)] unsafe {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        let url: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
        let op: Vec<u16> = "open\0".encode_utf16().collect();
        let result = ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), url.as_ptr(), std::ptr::null(), std::ptr::null(), 1);
        ensure!(result as isize > 32, "浏览器打开失败，请手动打开 http://127.0.0.1:18794");
    }
    #[cfg(target_os="macos")]
    { std::process::Command::new("open").arg(url).spawn()?; }
    #[cfg(all(not(windows),not(target_os="macos")))]
    { std::process::Command::new("xdg-open").arg(url).spawn()?; }
    Ok(())
}
