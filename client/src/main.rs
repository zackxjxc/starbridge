#![cfg_attr(
    all(windows, feature = "desktop", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use clap::Parser;
use fs2::FileExt;
use starbridge_client::ClientApp;
use starbridge_protocol::Role;
use std::{
    fs::File,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "starbridge", about = "StarBridge desktop node console")]
struct Args {
    #[arg(long, default_value = "controller")]
    role: String,
    /// Used only in headless mode.
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Run the local HTTP console without a desktop window.
    #[arg(long)]
    headless: bool,
    /// Compatibility alias for --headless.
    #[arg(long, hide = true)]
    no_browser: bool,
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
        "controller" => Role::Controller,
        "executor" => Role::Executor,
        _ => anyhow::bail!("--role 只能是 controller 或 executor"),
    };
    let headless = args.headless || args.no_browser || !cfg!(feature = "desktop");
    let root = match args.data_dir {
        Some(path) => std::path::absolute(path)?,
        None if headless => std::env::current_dir()?.join("state").join(role_name(role)),
        None => std::env::current_exe()?
            .parent()
            .ok_or_else(|| anyhow::anyhow!("无法确定程序目录"))?
            .join("state")
            .join(role_name(role)),
    };
    // Keep the lock open for the entire lifetime of this instance.
    let _instance = lock_data_dir(&root)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let app = runtime.block_on(ClientApp::load(role, root.clone()))?;
    runtime.block_on(app.heartbeat_loop());
    let result = if headless {
        runtime.block_on(serve(&app, args.listen))
    } else {
        #[cfg(feature = "desktop")]
        {
            tauri::async_runtime::set(runtime.handle().clone());
            starbridge_client::desktop::run(app.clone(), role, root)
        }
        #[cfg(not(feature = "desktop"))]
        unreachable!()
    };
    runtime.block_on(app.shutdown());
    result
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Controller => "controller",
        Role::Executor => "executor",
    }
}

fn lock_data_dir(root: &Path) -> anyhow::Result<File> {
    std::fs::create_dir_all(root)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("instance.lock"))?;
    file.try_lock_exclusive().map_err(|error| {
        anyhow::anyhow!(
            "无法锁定数据目录 {}，请检查是否已有实例运行：{error}",
            root.display()
        )
    })?;
    Ok(file)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_directory_is_exclusive_until_instance_exits() {
        let root = std::env::temp_dir().join(format!("starbridge-lock-{}", uuid::Uuid::new_v4()));
        let first = lock_data_dir(&root).unwrap();
        assert!(lock_data_dir(&root).is_err());
        drop(first);
        let second = lock_data_dir(&root).unwrap();
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }
}
