use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use starbridge_protocol::{
    AuthRequest, AuthResponse, ConnectionPolicy, DeviceInfo, DeviceRegistration, ErrorResponse,
    FrpRoute, HealthResponse, HeartbeatRequest, Role, SessionCreateRequest, SessionInfo,
    SessionPath, SessionStatus, SessionStatusRequest, new_token, now_unix,
};
use std::{
    path::{Path as FsPath, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::sync::{Mutex, RwLock};

const CLIENT_HTML: &str = include_str!("../../client-ui/index.html");

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClientConfig {
    pub server_url: String,
    pub display_name: String,
    pub ssh_alias: String,
    pub ssh_user: String,
    pub allow_relay: bool,
    pub direct_only: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server_url: "http://127.0.0.1:17850".to_string(),
            display_name: hostname(),
            ssh_alias: format!(
                "starbridge-{}",
                hostname().to_lowercase().replace([' ', '/', '\\'], "-")
            ),
            ssh_user: std::env::var("USERNAME")
                .or_else(|_| std::env::var("USER"))
                .unwrap_or_else(|_| "user".into()),
            allow_relay: true,
            direct_only: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Identity {
    pub node_id: String,
    pub device_token: String,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            node_id: starbridge_protocol::new_id(),
            device_token: new_token(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeStatus {
    pub frpc_path: String,
    pub frpc_available: bool,
    pub ssh_path: String,
    pub ssh_available: bool,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClientSnapshot {
    pub role: Role,
    pub logged_in: bool,
    pub username: Option<String>,
    pub server_url: String,
    pub node_id: String,
    pub display_name: String,
    pub devices: Vec<DeviceInfo>,
    pub session: Option<SessionView>,
    pub stage: String,
    pub last_error: Option<String>,
    pub runtime: RuntimeStatus,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionView {
    pub session_id: String,
    pub policy: ConnectionPolicy,
    pub path: SessionPath,
    pub status: SessionStatus,
    pub created_at: u64,
    pub expires_at: u64,
    pub failure_reason: Option<String>,
}

#[derive(Clone)]
pub struct ClientApp {
    inner: Arc<RwLock<Inner>>,
    http: reqwest::Client,
    frpc_process: Arc<Mutex<Option<tokio::process::Child>>>,
}

struct Inner {
    role: Role,
    root: PathBuf,
    config: ClientConfig,
    identity: Identity,
    auth: Option<AuthResponse>,
    devices: Vec<DeviceInfo>,
    session: Option<SessionInfo>,
    stage: String,
    last_error: Option<String>,
    runtime: RuntimeStatus,
}

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct ConnectForm {
    pub target_node_id: String,
    pub policy: ConnectionPolicy,
}

impl ClientApp {
    pub async fn load(role: Role, root: PathBuf) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(&root).await?;
        let config = load_json(&root.join("config.json"), ClientConfig::default()).await?;
        let identity = load_json(&root.join("identity.json"), Identity::default()).await?;
        let runtime = runtime_status(&root);
        Ok(Self {
            inner: Arc::new(RwLock::new(Inner {
                role,
                root,
                config,
                identity,
                auth: None,
                devices: Vec::new(),
                session: None,
                stage: "ready".into(),
                last_error: None,
                runtime,
            })),
            http: reqwest::Client::builder().no_proxy().build()?,
            frpc_process: Arc::new(Mutex::new(None)),
        })
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/", get(index))
            .route("/api/health", get(health))
            .route("/api/v1/state", get(state))
            .route("/api/v1/config", get(get_config).put(update_config))
            .route("/api/v1/login", post(login))
            .route("/api/v1/logout", post(logout))
            .route("/api/v1/connect", post(connect))
            .route("/api/v1/disconnect", delete(disconnect))
            .route("/api/v1/copy/nodes", get(copy_nodes))
            .with_state(self.clone())
    }

    pub async fn heartbeat_loop(&self) {
        let app = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                interval.tick().await;
                if let Err(error) = app.heartbeat_once().await {
                    let mut inner = app.inner.write().await;
                    if inner.auth.is_some() {
                        inner.last_error = Some(error.to_string());
                        inner.stage = "heartbeat_failed".into();
                    }
                }
            }
        });
    }

    async fn heartbeat_once(&self) -> anyhow::Result<()> {
        let (auth, url, node_id, role) = {
            let inner = self.inner.read().await;
            let auth = inner
                .auth
                .clone()
                .ok_or_else(|| anyhow::anyhow!("未登录"))?;
            (
                auth,
                inner.config.server_url.clone(),
                inner.identity.node_id.clone(),
                inner.role,
            )
        };
        let response = self
            .http
            .post(format!("{url}/api/v1/devices/heartbeat"))
            .bearer_auth(&auth.access_token)
            .json(&HeartbeatRequest {
                node_id: node_id.clone(),
                status: "online".into(),
            })
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<DeviceInfo>>()
            .await?;
        let assigned = if role == Role::Executor {
            self.http
                .get(format!("{url}/api/v1/sessions/assigned/{node_id}"))
                .bearer_auth(&auth.access_token)
                .send()
                .await?
                .error_for_status()?
                .json::<Vec<SessionInfo>>()
                .await?
        } else {
            Vec::new()
        };
        let mut inner = self.inner.write().await;
        inner.devices = response;
        inner.stage = "online".into();
        inner.last_error = None;
        let assigned_session = assigned.first().cloned();
        let should_start = assigned_session.as_ref().is_some_and(|session| {
            inner.session.as_ref().is_none_or(|old| {
                old.session_id != session.session_id
                    || (session.path == SessionPath::Relay && old.path != SessionPath::Relay)
            })
        });
        if let Some(session) = assigned_session {
            if !should_start {
                inner.session = Some(session.clone());
                match session.path {
                    SessionPath::Direct => {
                        inner.stage = "direct".into();
                        inner.last_error = None;
                    }
                    SessionPath::Relay => {
                        inner.stage = "frpc_running".into();
                    }
                    SessionPath::Unknown => {}
                }
            }
            if !should_start {
                return Ok(());
            }
            inner.session = Some(session.clone());
            drop(inner);
            match self
                .start_frpc_mode(&session, false, session.path == SessionPath::Relay)
                .await
            {
                Ok(()) => {
                    let path = if session.policy == ConnectionPolicy::RelayOnly {
                        SessionPath::Relay
                    } else {
                        SessionPath::Unknown
                    };
                    if let Ok(updated) =
                        update_remote_session(self, &session, SessionStatus::Connected, path, None)
                            .await
                    {
                        let mut inner = self.inner.write().await;
                        inner.session = Some(updated);
                    }
                }
                Err(error) => {
                    let mut inner = self.inner.write().await;
                    inner.stage = if error.to_string().contains("缺少 frpc") {
                        "runtime_missing"
                    } else {
                        "frpc_failed"
                    }
                    .into();
                    inner.last_error = Some(format!(
                        "执行端 frpc 路由启动失败：{error}。请检查 Windows Defender、杀毒软件或企业安全策略；StarBridge 不会自动修改白名单。"
                    ));
                    drop(inner);
                    let _ = update_remote_session(
                        self,
                        &session,
                        SessionStatus::Failed,
                        SessionPath::Unknown,
                        Some(error.to_string()),
                    )
                    .await;
                }
            }
        }
        Ok(())
    }

    async fn start_frpc(&self, session: &SessionInfo, controller: bool) -> anyhow::Result<()> {
        self.start_frpc_mode(session, controller, false).await
    }

    async fn start_frpc_mode(
        &self,
        session: &SessionInfo,
        controller: bool,
        force_relay: bool,
    ) -> anyhow::Result<()> {
        let route = session
            .route
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("中心没有返回 FRP 路由参数"))?;
        let (root, frpc_path) = {
            let inner = self.inner.read().await;
            (inner.root.clone(), inner.runtime.frpc_path.clone())
        };
        if !FsPath::new(&frpc_path).is_file() {
            anyhow::bail!("缺少 frpc: {frpc_path}");
        }
        let port = if controller {
            pick_port(22000, 22999)?
        } else {
            0
        };
        let config = frpc_config(route, session, controller, port, force_relay);
        let routes = root.join("routes");
        tokio::fs::create_dir_all(&routes).await?;
        let config_path = routes.join(format!("{}.toml", session.session_id));
        tokio::fs::write(&config_path, config).await?;
        let log_path = routes.join(format!("{}.log", session.session_id));
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let mut process = tokio::process::Command::new(&frpc_path);
        process
            .arg("-c")
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        let mut child = process
            .spawn()
            .map_err(|error| anyhow::anyhow!("启动 frpc 失败: {error}"))?;
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("frpc 启动后退出: {status}，日志见 {}", log_path.display());
        }
        let mut current = self.frpc_process.lock().await;
        if let Some(old) = current.as_mut() {
            let _ = old.kill().await;
            let _ = old.wait().await;
        }
        *current = Some(child);
        Ok(())
    }

    fn spawn_direct_fallback_monitor(&self, session: SessionInfo) {
        if session.policy != ConnectionPolicy::DirectPreferred {
            return;
        }
        let app = self.clone();
        tokio::spawn(async move {
            let root = { app.inner.read().await.root.clone() };
            let log_path = root
                .join("routes")
                .join(format!("{}.log", session.session_id));
            for _ in 0..60 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let log = match tokio::fs::read_to_string(&log_path).await {
                    Ok(log) => log,
                    Err(_) => continue,
                };
                match frpc_path_from_log(&log) {
                    Some(SessionPath::Direct) => {
                        if let Ok(updated) = update_remote_session(
                            &app,
                            &session,
                            SessionStatus::Connected,
                            SessionPath::Direct,
                            None,
                        )
                        .await
                        {
                            let mut inner = app.inner.write().await;
                            inner.session = Some(updated);
                            inner.stage = "direct".into();
                            inner.last_error = None;
                        }
                    }
                    Some(SessionPath::Relay) => {
                        if app.start_frpc_mode(&session, true, true).await.is_ok() {
                            if let Ok(updated) = update_remote_session(
                                &app,
                                &session,
                                SessionStatus::Connected,
                                SessionPath::Relay,
                                Some("XTCP 直连失败，已回退到 STCP 中转".into()),
                            )
                            .await
                            {
                                let mut inner = app.inner.write().await;
                                inner.session = Some(updated);
                                inner.stage = "relay_fallback".into();
                                inner.last_error = Some("XTCP 直连失败，已回退到 FRPS 中转".into());
                            }
                        }
                    }
                    _ => continue,
                }
                break;
            }
        });
    }

    pub async fn stop_frpc(&self) {
        let mut current = self.frpc_process.lock().await;
        if let Some(child) = current.as_mut() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        *current = None;
    }
}

async fn index() -> Html<&'static str> {
    Html(CLIENT_HTML)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        service: "starbridge-client",
        version: env!("CARGO_PKG_VERSION"),
        now: now_unix(),
    })
}

async fn state(State(app): State<ClientApp>) -> Json<ClientSnapshot> {
    let inner = app.inner.read().await;
    Json(snapshot(&inner))
}

async fn get_config(State(app): State<ClientApp>) -> Json<ClientConfig> {
    Json(app.inner.read().await.config.clone())
}

async fn update_config(
    State(app): State<ClientApp>,
    Json(config): Json<ClientConfig>,
) -> Result<Json<ClientConfig>, ApiError> {
    validate_config(&config)?;
    let mut inner = app.inner.write().await;
    inner.config = config;
    save_json(&inner.root.join("config.json"), &inner.config)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(inner.config.clone()))
}

async fn login(
    State(app): State<ClientApp>,
    Json(form): Json<LoginForm>,
) -> Result<Json<ClientSnapshot>, ApiError> {
    if form.username.trim().is_empty() || form.password.len() < 4 {
        return Err(ApiError::bad_request("请输入账户名称和至少 4 位密码"));
    }
    let (url, role, node_id, config, identity) = {
        let inner = app.inner.read().await;
        (
            inner.config.server_url.clone(),
            inner.role,
            inner.identity.node_id.clone(),
            inner.config.clone(),
            inner.identity.clone(),
        )
    };
    let auth = app
        .http
        .post(format!("{url}/api/v1/auth/login"))
        .json(&AuthRequest {
            username: form.username,
            password: form.password,
        })
        .send()
        .await
        .map_err(ApiError::network)?
        .error_for_status()
        .map_err(ApiError::from_response)?
        .json::<AuthResponse>()
        .await
        .map_err(ApiError::network)?;
    let registration = DeviceRegistration {
        node_id,
        device_token: identity.device_token.clone(),
        display_name: config.display_name,
        role,
        ssh_alias: config.ssh_alias,
        ssh_user: config.ssh_user,
        capabilities: vec!["ssh".into(), "sftp".into(), "scp".into()],
        allow_relay: config.allow_relay,
        direct_only: config.direct_only,
        version: env!("CARGO_PKG_VERSION").into(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        hostname: hostname(),
    };
    let devices = app
        .http
        .post(format!("{url}/api/v1/devices/register"))
        .bearer_auth(&auth.access_token)
        .json(&registration)
        .send()
        .await
        .map_err(ApiError::network)?
        .error_for_status()
        .map_err(ApiError::from_response)?
        .json::<Vec<DeviceInfo>>()
        .await
        .map_err(ApiError::network)?;
    let mut inner = app.inner.write().await;
    inner.auth = Some(auth);
    inner.devices = devices;
    inner.stage = "online".into();
    inner.last_error = None;
    save_json(&inner.root.join("identity.json"), &identity)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(snapshot(&inner)))
}

async fn logout(State(app): State<ClientApp>) -> Json<ClientSnapshot> {
    app.stop_frpc().await;
    let mut inner = app.inner.write().await;
    inner.auth = None;
    inner.session = None;
    inner.stage = "ready".into();
    Json(snapshot(&inner))
}

async fn connect(
    State(app): State<ClientApp>,
    Json(form): Json<ConnectForm>,
) -> Result<Json<ClientSnapshot>, ApiError> {
    let (auth, url, role, runtime) = {
        let inner = app.inner.read().await;
        (
            inner
                .auth
                .clone()
                .ok_or_else(|| ApiError::unauthorized("请先登录"))?,
            inner.config.server_url.clone(),
            inner.role,
            inner.runtime.clone(),
        )
    };
    if role != Role::Controller {
        return Err(ApiError::bad_request("只有控制端可以发起连接"));
    }
    let info = app
        .http
        .post(format!("{url}/api/v1/sessions"))
        .bearer_auth(&auth.access_token)
        .json(&SessionCreateRequest {
            target_node_id: form.target_node_id,
            policy: form.policy,
        })
        .send()
        .await
        .map_err(ApiError::network)?
        .error_for_status()
        .map_err(ApiError::from_response)?
        .json::<SessionInfo>()
        .await
        .map_err(ApiError::network)?;
    {
        let mut inner = app.inner.write().await;
        inner.session = Some(info.clone());
    }
    if !runtime.frpc_available {
        let mut inner = app.inner.write().await;
        inner.stage = "runtime_missing".into();
        inner.last_error = Some(
            "未找到 runtime/frpc.exe。请先按 vendor/manifest.toml 准备并校验 FRP 运行时。".into(),
        );
        drop(inner);
        if let Ok(updated) = update_remote_session(
            &app,
            &info,
            SessionStatus::Failed,
            SessionPath::Unknown,
            Some("frpc runtime missing".into()),
        )
        .await
        {
            let mut inner = app.inner.write().await;
            inner.session = Some(updated);
        }
    } else if let Err(error) = probe_frpc(&runtime.frpc_path) {
        let mut inner = app.inner.write().await;
        inner.stage = "frpc_blocked".into();
        inner.last_error = Some(format!(
            "frpc 启动检查失败：{error}。请检查 Windows Defender、杀毒软件或企业安全策略；StarBridge 不会自动修改白名单。"
        ));
        drop(inner);
        let _ = update_remote_session(
            &app,
            &info,
            SessionStatus::Failed,
            SessionPath::Unknown,
            Some(error.to_string()),
        )
        .await;
    } else {
        match app.start_frpc(&info, true).await {
            Ok(()) => {
                let path = if info.policy == ConnectionPolicy::RelayOnly {
                    SessionPath::Relay
                } else {
                    SessionPath::Unknown
                };
                if let Ok(updated) =
                    update_remote_session(&app, &info, SessionStatus::Connected, path, None).await
                {
                    let mut inner = app.inner.write().await;
                    inner.session = Some(updated);
                    inner.stage = "frpc_running".into();
                    inner.last_error = None;
                }
                app.spawn_direct_fallback_monitor(info.clone());
            }
            Err(error) => {
                let mut inner = app.inner.write().await;
                inner.stage = "frpc_failed".into();
                inner.last_error = Some(format!(
                    "frpc 路由启动失败：{error}。请检查 Windows Defender、杀毒软件或企业安全策略；StarBridge 不会自动修改白名单。"
                ));
                drop(inner);
                let _ = update_remote_session(
                    &app,
                    &info,
                    SessionStatus::Failed,
                    SessionPath::Unknown,
                    Some(error.to_string()),
                )
                .await;
            }
        }
    }
    let inner = app.inner.read().await;
    Ok(Json(snapshot(&inner)))
}

async fn disconnect(State(app): State<ClientApp>) -> Result<Json<ClientSnapshot>, ApiError> {
    let (auth, url, session) = {
        let inner = app.inner.read().await;
        (
            inner.auth.clone(),
            inner.config.server_url.clone(),
            inner.session.clone(),
        )
    };
    if let (Some(auth), Some(session)) = (auth, session) {
        app.http
            .delete(format!("{url}/api/v1/sessions/{}", session.session_id))
            .bearer_auth(&auth.access_token)
            .send()
            .await
            .map_err(ApiError::network)?
            .error_for_status()
            .map_err(ApiError::from_response)
            .map(|_| ())?;
    }
    app.stop_frpc().await;
    let mut inner = app.inner.write().await;
    inner.session = None;
    inner.stage = if inner.auth.is_some() {
        "online"
    } else {
        "ready"
    }
    .into();
    inner.last_error = None;
    Ok(Json(snapshot(&inner)))
}

async fn copy_nodes(State(app): State<ClientApp>) -> Json<serde_json::Value> {
    let inner = app.inner.read().await;
    let text = inner
        .devices
        .iter()
        .filter(|d| d.role == Role::Executor && d.online)
        .map(|d| {
            format!(
                "{} ({}) · {} · {}",
                d.display_name,
                d.ssh_alias,
                d.os,
                d.capabilities.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Json(serde_json::json!({ "text": text }))
}

async fn update_remote_session(
    app: &ClientApp,
    info: &SessionInfo,
    status: SessionStatus,
    path: SessionPath,
    failure_reason: Option<String>,
) -> anyhow::Result<SessionInfo> {
    let (auth, url) = {
        let inner = app.inner.read().await;
        (
            inner
                .auth
                .clone()
                .ok_or_else(|| anyhow::anyhow!("not logged in"))?,
            inner.config.server_url.clone(),
        )
    };
    Ok(app
        .http
        .post(format!("{url}/api/v1/sessions/{}/status", info.session_id))
        .bearer_auth(auth.access_token)
        .json(&SessionStatusRequest {
            status,
            path,
            failure_reason,
        })
        .send()
        .await?
        .error_for_status()?
        .json::<SessionInfo>()
        .await?)
}

fn snapshot(inner: &Inner) -> ClientSnapshot {
    ClientSnapshot {
        role: inner.role,
        logged_in: inner.auth.is_some(),
        username: inner.auth.as_ref().map(|a| a.username.clone()),
        server_url: inner.config.server_url.clone(),
        node_id: inner.identity.node_id.clone(),
        display_name: inner.config.display_name.clone(),
        devices: inner.devices.clone(),
        session: inner.session.as_ref().map(|session| SessionView {
            session_id: session.session_id.clone(),
            policy: session.policy,
            path: session.path,
            status: session.status,
            created_at: session.created_at,
            expires_at: session.expires_at,
            failure_reason: session.failure_reason.clone(),
        }),
        stage: inner.stage.clone(),
        last_error: inner.last_error.clone(),
        runtime: inner.runtime.clone(),
        updated_at: now_unix(),
    }
}

fn runtime_status(root: &FsPath) -> RuntimeStatus {
    let local_runtime = root.join("runtime");
    let shared_runtime = root
        .parent()
        .map(|parent| parent.join("runtime"))
        .unwrap_or_else(|| local_runtime.clone());
    let runtime = if local_runtime.exists() || !shared_runtime.exists() {
        local_runtime
    } else {
        shared_runtime
    };
    let frpc = runtime.join(if cfg!(windows) { "frpc.exe" } else { "frpc" });
    let ssh = runtime.join(if cfg!(windows) { "ssh.exe" } else { "ssh" });
    let frpc_available = frpc.is_file();
    let ssh_available = ssh.is_file() || command_available("ssh");
    RuntimeStatus {
        frpc_path: frpc.display().to_string(),
        frpc_available,
        ssh_path: ssh.display().to_string(),
        ssh_available,
        message: if frpc_available {
            "FRP 运行时已找到".into()
        } else {
            "缺少 frpc，连接只能验证控制面".into()
        },
    }
}

fn probe_frpc(path: &str) -> anyhow::Result<String> {
    let output = std::process::Command::new(path)
        .arg("--version")
        .output()
        .map_err(|error| anyhow::anyhow!("无法启动 {path}: {error}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "进程退出码 {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let output_text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if output_text.is_empty() {
        String::from_utf8_lossy(&output.stderr).trim().to_string()
    } else {
        output_text
    })
}

fn pick_port(start: u16, end: u16) -> anyhow::Result<u16> {
    for port in start..=end {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!("本地 visitor 端口范围 {start}-{end} 已耗尽")
}

fn frpc_config(
    route: &FrpRoute,
    session: &SessionInfo,
    controller: bool,
    visitor_port: u16,
    force_relay: bool,
) -> String {
    let mode = if force_relay || session.policy == ConnectionPolicy::RelayOnly {
        "stcp"
    } else {
        "xtcp"
    };
    let mut config = format!(
        "serverAddr = \"{}\"\nserverPort = {}\nauth.method = \"token\"\nauth.token = \"{}\"\n\n",
        route.server_addr,
        route.server_port,
        route.auth_token.replace('"', "")
    );
    if controller {
        config.push_str(&format!(
            "[[visitors]]\nname = \"visitor-{}\"\ntype = \"{}\"\nserverName = \"{}\"\nsecretKey = \"{}\"\nbindAddr = \"127.0.0.1\"\nbindPort = {}\n",
            session.session_id,
            mode,
            route.proxy_name,
            route.secret_key,
            visitor_port
        ));
    } else {
        config.push_str(&format!(
            "[[proxies]]\nname = \"{}\"\ntype = \"{}\"\nsecretKey = \"{}\"\nlocalIP = \"127.0.0.1\"\nlocalPort = {}\n",
            route.proxy_name,
            mode,
            route.secret_key,
            std::env::var("STARBRIDGE_SSH_PORT").ok().and_then(|value| value.parse().ok()).unwrap_or(22u16)
        ));
    }
    config
}

fn command_available(name: &str) -> bool {
    let directories: Vec<_> = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .collect();
    directories.into_iter().any(|dir| {
        dir.join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.into()
        })
        .is_file()
    })
}

fn frpc_path_from_log(log: &str) -> Option<SessionPath> {
    if log.contains("establishing nat hole connection successful") {
        Some(SessionPath::Direct)
    } else if log.contains("make hole failed")
        || log.contains("connect to proxy error")
        || log.contains("visitor connect failed")
    {
        Some(SessionPath::Relay)
    } else {
        None
    }
}

fn validate_config(config: &ClientConfig) -> Result<(), ApiError> {
    if config.server_url.trim().is_empty()
        || config.display_name.trim().is_empty()
        || config.ssh_alias.trim().is_empty()
        || config.ssh_user.trim().is_empty()
    {
        return Err(ApiError::bad_request(
            "中心 URL、显示名称、SSH 别名和 SSH 用户不能为空",
        ));
    }
    if config.direct_only && config.allow_relay {
        return Err(ApiError::bad_request("本机只允许直连时不能允许中继"));
    }
    Ok(())
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "starbridge-node".into())
}

async fn load_json<T: for<'de> Deserialize<'de> + Serialize>(
    path: &FsPath,
    default: T,
) -> anyhow::Result<T> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            save_json(path, &default).await?;
            Ok(default)
        }
        Err(error) => Err(error.into()),
    }
}

async fn save_json<T: Serialize>(path: &FsPath, value: &T) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let pending = path.with_extension("pending");
    tokio::fs::write(&pending, bytes).await?;
    tokio::fs::rename(pending, path).await?;
    Ok(())
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }
    fn internal(error: anyhow::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
    fn network(error: reqwest::Error) -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            format!("中心服务连接失败：{error}"),
        )
    }
    fn from_response(error: reqwest::Error) -> Self {
        Self::new(
            error.status().unwrap_or(StatusCode::BAD_GATEWAY),
            error.to_string(),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(policy: ConnectionPolicy) -> SessionInfo {
        SessionInfo {
            session_id: "session-id".into(),
            controller_node_id: "controller".into(),
            target_node_id: "executor".into(),
            policy,
            path: SessionPath::Unknown,
            status: SessionStatus::Negotiating,
            created_at: 1,
            expires_at: 2,
            failure_reason: None,
            route: Some(FrpRoute {
                server_addr: "127.0.0.1".into(),
                server_port: 17851,
                auth_token: "token".into(),
                proxy_name: "proxy".into(),
                secret_key: "secret".into(),
                remote_port: 20000,
            }),
        }
    }

    #[test]
    fn relay_route_uses_stcp_and_hides_nothing_from_runtime_config() {
        let info = session(ConnectionPolicy::RelayOnly);
        let config = frpc_config(info.route.as_ref().unwrap(), &info, true, 22000, false);
        assert!(config.contains("type = \"stcp\""));
        assert!(config.contains("bindPort = 22000"));
        assert!(config.contains("auth.token = \"token\""));
    }

    #[test]
    fn preferred_route_starts_with_xtcp() {
        let info = session(ConnectionPolicy::DirectPreferred);
        let config = frpc_config(info.route.as_ref().unwrap(), &info, false, 0, false);
        assert!(config.contains("type = \"xtcp\""));
        assert!(config.contains("localPort = 22"));
    }

    #[test]
    fn xtcp_success_log_reports_direct_path() {
        assert_eq!(
            frpc_path_from_log("establishing nat hole connection successful"),
            Some(SessionPath::Direct)
        );
    }

    #[test]
    fn xtcp_failure_log_reports_relay_path() {
        assert_eq!(
            frpc_path_from_log("visitor connect failed: timeout"),
            Some(SessionPath::Relay)
        );
        assert_eq!(frpc_path_from_log("login to server success"), None);
    }
}
