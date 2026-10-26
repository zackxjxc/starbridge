use clap::Parser;
use starbridge_server::{AppState, DiagnosticIntegration, app, diagnostic_plugin_app};
use std::{net::SocketAddr, path::PathBuf, process::Stdio};
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "starbridge-server", version, about = "StarBridge central server")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:17850")]
    listen: SocketAddr,
    /// Optional path to a verified frps binary. The server continues in control-plane-only mode when absent.
    #[arg(long)]
    frps_path: Option<PathBuf>,
    #[arg(long, default_value_t = 17851)]
    frps_port: u16,
    /// Address advertised to clients in session routes (the address they use to reach frps).
    #[arg(
        long = "frps-addr",
        alias = "frps-advertised-addr",
        default_value = "127.0.0.1"
    )]
    frps_addr: String,
    /// Local address where the managed frps process listens.
    #[arg(long, default_value = "127.0.0.1")]
    frps_bind_addr: String,
    #[arg(long, default_value = "state/server")]
    data_dir: PathBuf,
    #[arg(long)]
    frps_token: Option<String>,
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
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    info!(address = %listener.local_addr()?, "StarBridge central server listening");
    let frps_token = frp_token(&args);
    let state = AppState::with_frp_and_data(
            args.frps_addr.clone(),
            args.frps_port,
            frps_token,
            args.data_dir.clone(),
        );
    let managed = if args.frps_path.as_ref().is_some_and(|path| path.is_file()) {
        let plugin_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let plugin_address = plugin_listener.local_addr()?;
        let admin_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let admin_port = admin_listener.local_addr()?.port();
        let integration = DiagnosticIntegration {
            admin_url: format!("http://127.0.0.1:{admin_port}"), admin_user: "starbridge".into(),
            admin_password: starbridge_protocol::new_token(), plugin_token: starbridge_protocol::new_token(),
        };
        let router = diagnostic_plugin_app(state.clone(), &integration.plugin_token);
        let task = tokio::spawn(async move { axum::serve(plugin_listener, router).await });
        drop(admin_listener);
        Some((integration, plugin_address, admin_port, task))
    } else { None };
    let mut frps = start_frps(&args, managed.as_ref().map(|(integration, address, port, _)| (integration, *address, *port))).await?;
    if let Some((integration, _, _, _)) = &managed {
        state.configure_diagnostics(integration.clone()).await;
    }
    let maintenance = state.start_diagnostic_maintenance();
    let result = axum::serve(listener, app(state))
    .with_graceful_shutdown(shutdown_signal())
    .await;
    maintenance.abort();
    if let Some(child) = frps.as_mut() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    if let Some((_, _, _, task)) = managed { task.abort(); }
    result?;
    Ok(())
}

/// 等待 Ctrl+C；Unix 上同时处理 SIGTERM（kill、systemctl stop）和 SIGHUP，确保 frps 被清理。
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

async fn start_frps(args: &Args, diagnostic: Option<(&DiagnosticIntegration, SocketAddr, u16)>) -> anyhow::Result<Option<tokio::process::Child>> {
    let Some(path) = args.frps_path.as_ref() else {
        tracing::warn!("未配置 frps，中心服务以控制面模式启动；需要数据面时传入 --frps-path");
        return Ok(None);
    };
    if !path.is_file() {
        tracing::warn!(path = %path.display(), "frps 文件不存在，中心服务以控制面模式启动");
        return Ok(None);
    }
    let config_dir = args.data_dir.clone();
    tokio::fs::create_dir_all(&config_dir).await?;
    let config_path = config_dir.join("frps.toml");
    let token = frp_token(args);
    let mut config = render_frps_config(&args.frps_bind_addr, args.frps_port, &token);
    if let Some((integration, address, port)) = diagnostic {
        config.push_str(&format!(
            "webServer.addr = \"127.0.0.1\"\nwebServer.port = {port}\nwebServer.user = \"starbridge\"\nwebServer.password = \"{}\"\n\n[[httpPlugins]]\nname = \"starbridge-diagnostics\"\naddr = \"{address}\"\npath = \"/frp/{}\"\nops = [\"Login\", \"NewProxy\", \"NewUserConn\", \"NewWorkConn\", \"Ping\", \"CloseProxy\"]\n",
            integration.admin_password, integration.plugin_token,
        ));
    }
    tokio::fs::write(&config_path, config).await?;
    let mut command = tokio::process::Command::new(path);
    command
        .arg("-c")
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = command
        .spawn()
        .map_err(|error| anyhow::anyhow!("启动 frps 失败: {error}"))?;
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    if let Some(status) = child.try_wait()? {
        anyhow::bail!("frps 启动后退出: {status}");
    }
    tracing::info!(path = %path.display(), port = args.frps_port, "frps 已启动");
    Ok(Some(child))
}

fn render_frps_config(bind_addr: &str, bind_port: u16, token: &str) -> String {
    format!(
        "bindAddr = \"{}\"\nbindPort = {}\nauth.method = \"token\"\nauth.token = \"{}\"\n",
        bind_addr,
        bind_port,
        token.replace('"', "")
    )
}

fn frp_token(args: &Args) -> String {
    args.frps_token
        .clone()
        .or_else(|| std::env::var("STARBRIDGE_FRP_TOKEN").ok())
        .unwrap_or_else(|| "starbridge-dev-token".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn bind_and_advertised_addresses_are_independent() {
        let args = Args::try_parse_from([
            "starbridge-server",
            "--frps-bind-addr",
            "127.0.0.1",
            "--frps-addr",
            "frps.example.test",
        ])
        .expect("valid arguments");
        let config = render_frps_config(&args.frps_bind_addr, args.frps_port, "token");
        assert!(config.contains("bindAddr = \"127.0.0.1\""));
        assert!(!config.contains("frps.example.test"));
        assert_eq!(args.frps_addr, "frps.example.test");
    }

    #[test]
    fn advertised_address_alias_is_supported() {
        let args = Args::try_parse_from([
            "starbridge-server",
            "--frps-advertised-addr",
            "relay.example.test",
        ])
        .expect("valid arguments");
        assert_eq!(args.frps_addr, "relay.example.test");
    }
}
