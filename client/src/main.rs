use clap::Parser;
use starbridge_client::ClientApp;
use starbridge_protocol::Role;
use std::{net::SocketAddr, path::PathBuf};
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "starbridge", about = "StarBridge local node console")]
struct Args {
    #[arg(long, default_value = "controller")]
    role: String,
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
    #[arg(long)]
    data_dir: Option<PathBuf>,
    #[arg(long)]
    no_browser: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    let root = args.data_dir.unwrap_or_else(|| {
        PathBuf::from("state").join(match role {
            Role::Controller => "controller",
            Role::Executor => "executor",
        })
    });
    let app = ClientApp::load(role, root).await?;
    app.heartbeat_loop().await;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    let address = listener.local_addr()?;
    let url = format!("http://{address}");
    info!(%url, role = ?role, "StarBridge local console listening");
    if !args.no_browser {
        let _ = webbrowser::open(&url);
    }
    axum::serve(listener, app.router())
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    app.stop_frpc().await;
    app.stop_sshd().await;
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
