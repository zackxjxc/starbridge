#![cfg_attr(
    all(windows, feature = "desktop", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use clap::Parser;
use starbridge_client::{ClientApp, data_directory, profiles};
use starbridge_protocol::Role;
use std::{
    net::SocketAddr,
    path::PathBuf,
};
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "starbridge", version, about = "StarBridge desktop node console")]
struct Args {
    /// Legacy role names are accepted only to locate existing data directories.
    #[arg(long, default_value = "peer", hide = true)]
    role: String,
    /// Used only in headless mode.
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
    /// Run an independent test identity in this directory, isolated from the default device.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Import one explicitly selected old profile into an empty default user directory.
    #[arg(long, conflicts_with = "data_dir")]
    import_data_dir: Option<PathBuf>,
    /// Run the local HTTP console without a desktop window.
    #[arg(long)]
    headless: bool,
    /// Compatibility alias for --headless.
    #[arg(long, hide = true)]
    no_browser: bool,
    /// Internal profile selector used by the account manager.
    #[arg(long, hide = true, conflicts_with_all = ["data_dir", "import_data_dir", "new_profile"])]
    profile_id: Option<String>,
    /// Open a fresh login profile.
    #[arg(long, hide = true, conflicts_with_all = ["data_dir", "import_data_dir", "profile_id"])]
    new_profile: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("StarBridge 启动失败：{error:#}");
        #[cfg(all(windows, feature = "desktop", not(debug_assertions)))]
        if !std::env::args().any(|arg| matches!(arg.as_str(), "--headless" | "--no-browser")) {
            show_error(&format!("StarBridge 启动失败：{error:#}"));
        }
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(false)
        .init();
    let args = Args::parse();
    let role = match args.role.to_ascii_lowercase().as_str() {
        "peer" | "controller" | "executor" => Role::Peer,
        _ => anyhow::bail!("不支持的兼容启动参数；请省略 --role"),
    };
    let headless = args.headless || args.no_browser || !cfg!(feature = "desktop");
    let executable = std::env::current_exe()?;
    let executable_dir = executable.parent().ok_or_else(|| anyhow::anyhow!("无法确定程序目录"))?;
    let _instance = if args.data_dir.is_some() || args.import_data_dir.is_some() {
        data_directory::prepare_with_import(args.data_dir, args.import_data_dir, &[executable_dir.to_path_buf(), std::env::current_dir()?])?
    } else {
        profiles::prepare_auto(args.profile_id.as_deref(), args.new_profile)?.directory
    };
    let root = _instance.root.clone();
    let runtime = tokio::runtime::Runtime::new()?;
    let loaded = runtime.block_on(ClientApp::load(role, root.clone()));
    let core = loaded.as_ref().ok().cloned();
    if let Some(app) = &core { runtime.block_on(app.heartbeat_loop()); }
    let result = if headless {
        let app = loaded?;
        runtime.block_on(serve(&app, args.listen))
    } else {
        #[cfg(feature = "desktop")]
        {
            tauri::async_runtime::set(runtime.handle().clone());
            starbridge_client::desktop::run(loaded, role, root)
        }
        #[cfg(not(feature = "desktop"))]
        unreachable!()
    };
    if let Some(app) = core { runtime.block_on(app.shutdown()); }
    result
}

async fn serve(app: &ClientApp, listen: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let address = listener.local_addr()?;
    info!(url = %format!("http://{address}"), "StarBridge headless console listening");
    axum::serve(listener, app.router())
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// 等待 Ctrl+C；Unix 上同时处理 kill 默认发送的 SIGTERM 和关闭终端时的 SIGHUP，确保子进程被清理。
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut hangup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            tokio::signal::ctrl_c().await.ok();
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
            _ = hangup.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.ok();
}

#[cfg(all(windows, feature = "desktop", not(debug_assertions)))]
fn show_error(message: &str) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(
            window: *mut std::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            flags: u32,
        ) -> i32;
    }
    let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "StarBridge".encode_utf16().chain(Some(0)).collect();
    // Both buffers remain valid and NUL terminated throughout this synchronous call.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), 0x10);
    }
}
