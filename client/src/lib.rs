use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use starbridge_protocol::{
    AuthRequest, AuthResponse, ConnectionPolicy, DeviceInfo, DeviceRegistration,
    DeviceRegistrationResponse, DeviceRebindRequest, ErrorResponse,
    FrpRoute, HealthResponse, HeartbeatRequest, Role, SessionCreateRequest, SessionInfo,
    SessionHistoryPage, SessionHistoryQuery,
    SessionPath, SessionStatus, SessionStatusRequest, SshPrivilege, new_token, now_unix,
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path as FsPath, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Mutex, RwLock};

mod account;
mod identity;
mod secure_store;
mod profile;
#[cfg(feature = "desktop")]
mod startup;
pub mod data_directory;
pub use identity::Identity;
mod connections;
mod diagnostics;
mod history;
mod enrollment;
mod process;
use account::*;
use connections::*;
use diagnostics::*;
use history::*;

#[cfg(feature = "desktop")]
pub mod desktop;

const CLIENT_HTML: &str = include_str!("../../client-ui/index.html");
const CLIENT_JS: &str = include_str!("../../client-ui/app.js");
const CLIENT_I18N: &str = include_str!("../../client-ui/i18n.js");
const CLIENT_CSS: &str = include_str!("../../client-ui/style.css");
pub const DEFAULT_SERVER_URL: &str = "https://starbridge.zackxjxc.de5.net";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ClientConfig {
    pub server_url: String,
    pub display_name: String,
    pub ssh_alias: String,
    pub ssh_user: String,
    pub allow_relay: bool,
    pub direct_only: bool,
    pub purpose: String,
    pub last_username: String,
    pub remember_login: bool,
    pub connection_policy: ConnectionPolicy,
    pub rebuild_relay: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server_url: std::env::var("STARBRIDGE_SERVER_URL").unwrap_or_else(|_| DEFAULT_SERVER_URL.to_string()),
            display_name: hostname(),
            ssh_alias: format!(
                "starbridge-{}",
                hostname().to_lowercase().replace([' ', '/', '\\'], "-")
            ),
            ssh_user: current_user(),
            allow_relay: true,
            direct_only: false,
            purpose: String::new(),
            last_username: String::new(),
            remember_login: true,
            connection_policy: ConnectionPolicy::DirectPreferred,
            rebuild_relay: false,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeStatus {
    pub frpc_path: String,
    pub frpc_available: bool,
    pub ssh_path: String,
    pub ssh_available: bool,
    #[serde(skip)]
    ssh_from_system: bool,
    pub message: String,
}

/// 本机同时具备 SSH 登录密钥和接收连接的服务能力。
#[derive(Clone, Debug, Default, Serialize)]
pub struct SshStatus {
    pub initialization: String,
    pub privilege: SshPrivilege,
    pub port: Option<u16>,
    pub public_key: String,
    pub host_key: String,
    /// 内置 sshd 只接受运行它的系统账户登录；非空时覆盖配置里的 SSH 用户。
    pub login_user: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClientSnapshot {
    pub role: Role,
    pub logged_in: bool,
    pub username: Option<String>,
    pub server_url: String,
    pub node_id: String,
    pub instance_id: String,
    pub display_name: String,
    pub devices: Vec<DeviceInfo>,
    pub session: Option<SessionView>,
    pub sessions: Vec<SessionView>,
    pub stage: String,
    pub last_error: Option<String>,
    pub device_rebind_required: bool,
    pub enrollment_candidates: Vec<starbridge_protocol::EnrollmentCandidate>,
    pub runtime: RuntimeStatus,
    pub ssh: SshStatus,
    pub ssh_command: Option<String>,
    pub updated_at: u64,
    pub remember_login: bool,
    pub last_username: String,
    pub purpose: String,
    pub platform: String,
    pub device_operations: Vec<DeviceOperation>,
    pub batch: Option<BatchView>,
    pub self_test: TestView,
    pub allow_relay: bool,
    pub direct_only: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionView {
    pub session_id: String,
    pub initiator_node_id: String,
    pub target_node_id: String,
    pub policy: ConnectionPolicy,
    pub path: SessionPath,
    pub status: SessionStatus,
    pub created_at: u64,
    pub expires_at: u64,
    pub failure_reason: Option<String>,
    pub ssh_command: Option<String>,
    pub ssh_ready: bool,
    pub verified_at: Option<u64>,
}

#[derive(Clone)]
pub struct ClientApp {
    inner: Arc<RwLock<Inner>>,
    http: reqwest::Client,
    frpc_processes: Arc<Mutex<HashMap<String, tokio::process::Child>>>,
    session_operations: Arc<Mutex<()>>,
    sshd_process: Arc<Mutex<Option<tokio::process::Child>>>,
    shutting_down: Arc<AtomicBool>,
    background_tasks: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    initialization: Arc<Mutex<()>>,
    heartbeat_gate: Arc<Mutex<()>>,
    device_updates: Arc<Mutex<()>>,
    incoming_gate: Arc<Mutex<()>>,
    route_start_gate: Arc<Mutex<()>>,
    diagnostic_gate: Arc<Mutex<()>>,
    connection_slots: Arc<tokio::sync::Semaphore>,
    diagnostic_cancel: Arc<AtomicBool>,
    process_job: Arc<process::ProcessJob>,
}

struct Inner {
    role: Role,
    root: PathBuf,
    config: ClientConfig,
    identity: Identity,
    auth: Option<AuthResponse>,
    devices: Vec<DeviceInfo>,
    sessions: HashMap<String, SessionInfo>,
    pending_closes: HashSet<String>,
    ended_sessions: HashSet<String>,
    stage: String,
    last_error: Option<String>,
    runtime: RuntimeStatus,
    ssh: SshStatus,
    visitor_ports: HashMap<String, u16>,
    route_generations: HashMap<String, u64>,
    verified_sessions: HashMap<String, u64>,
    operations: HashMap<String, DeviceOperation>,
    connection_generations: HashMap<String, u64>,
    blocked_peers: HashSet<String>,
    batch: Option<BatchRecord>,
    self_test: TestView,
    auth_generation: u64,
    closing_account: bool,
    device_rebind_required: bool,
    instance_id: String,
    pending_enrollment: Option<starbridge_protocol::DeviceEnrollmentRequest>,
    enrollment_candidates: Vec<starbridge_protocol::EnrollmentCandidate>,
}

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
    #[serde(default = "default_true")]
    pub remember: bool,
}

fn default_true() -> bool { true }

#[derive(Debug, Deserialize)]
pub struct ConnectForm {
    pub target_node_id: String,
    pub policy: ConnectionPolicy,
}

impl ClientApp {
    pub async fn load(_role: Role, root: PathBuf) -> anyhow::Result<Self> {
        Self::load_with_interaction(_role, root, secure_store::Interaction::Silent).await
    }

    pub(crate) async fn load_with_interaction(_role: Role, root: PathBuf, interaction: secure_store::Interaction) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(&root).await?;
        let mut config = load_json(&root.join("config.json"), ClientConfig::default()).await
            .map_err(|error| error.context("本机配置无法读取，请先检查配置后重试"))?;
        let mut identity = identity::load_identity_with_interaction(&root, interaction).await?;
        let loaded_auth = load_auth_with_interaction(&root, config.remember_login, interaction).await?;
        let auth = loaded_auth.auth;
        if let Some(auth) = &auth {
            identity.select_account(&account_scope(&config.server_url, &auth.username));
            identity::save_identity_with_interaction(&root, &identity, interaction).await?;
        }
        if config.last_username.is_empty() {
            if let Some(auth) = &auth { config.last_username = auth.username.clone(); }
        }
        let mut self_test = load_json(&root.join("last-test.json"), TestView::default()).await?;
        if matches!(self_test.state.as_str(), "running" | "cleaning") {
            self_test.state = "unverified".into();
            self_test.message = "上次测试未完成，已停止恢复；请重新测试".into();
        }
        if !diagnostics::recover_test_files(&root).await {
            self_test.state="failed".into();self_test.message="上次测试的临时文件清理未完成，请检查数据目录权限后重试".into();
        }
        let pending_closes = load_json(&root.join("pending-closes.json"), HashSet::new()).await?;
        let restoring = auth.is_some();
        let runtime = runtime_status(&root).await;
        let instance_id = new_token();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-starbridge-instance-id", instance_id.parse()?);
        let app = Self {
            inner: Arc::new(RwLock::new(Inner {
                role: Role::Peer,
                root,
                config,
                identity,
                auth,
                devices: Vec::new(),
                sessions: HashMap::new(),
                ended_sessions: pending_closes.clone(), pending_closes,
                stage: if restoring { "restoring" } else { "ready" }.into(),
                last_error: loaded_auth.cache_recovered.then(|| "已保留无法读取的登录缓存，请重新登录。".into()),
                runtime,
                ssh: SshStatus { initialization: "not_initialized".into(), message: "连接组件将在需要时启动".into(), ..Default::default() },
                visitor_ports: HashMap::new(),
                route_generations: HashMap::new(),
                verified_sessions: HashMap::new(), operations: HashMap::new(),
                connection_generations: HashMap::new(), blocked_peers: HashSet::new(),
                batch: None, self_test, auth_generation: 0, closing_account: false,
                device_rebind_required: false,
                instance_id, pending_enrollment: None, enrollment_candidates: Vec::new(),
            })),
            http: reqwest::Client::builder().no_proxy().default_headers(headers).timeout(std::time::Duration::from_secs(15)).build()?,
            frpc_processes: Arc::new(Mutex::new(HashMap::new())),
            session_operations: Arc::new(Mutex::new(())),
            sshd_process: Arc::new(Mutex::new(None)),
            shutting_down: Arc::new(AtomicBool::new(false)),
            background_tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
            initialization: Arc::new(Mutex::new(())), heartbeat_gate: Arc::new(Mutex::new(())),
            device_updates: Arc::new(Mutex::new(())),
            incoming_gate: Arc::new(Mutex::new(())),
            route_start_gate: Arc::new(Mutex::new(())),
            diagnostic_gate: Arc::new(Mutex::new(())),
            connection_slots: Arc::new(tokio::sync::Semaphore::new(3)),
            diagnostic_cancel: Arc::new(AtomicBool::new(false)),
            process_job: Arc::new(process::ProcessJob::new()?),
        };
        Ok(app)
    }

    async fn ensure_ssh_ready(&self) -> anyhow::Result<()> {
        let _gate = self.initialization.lock().await;
        if self.shutting_down.load(Ordering::Acquire) { anyhow::bail!("客户端正在退出"); }
        {
            let inner=self.inner.read().await;
            if inner.auth.is_none() || inner.closing_account {anyhow::bail!("请先登录");}
        }
        if self.inner.read().await.ssh.initialization == "ready" { return Ok(()); }
        let (root, runtime) = {
            let mut inner = self.inner.write().await;
            inner.ssh.initialization = "initializing".into();
            inner.ssh.message = "正在准备 SSH 连接组件".into();
            (inner.root.clone(), inner.runtime.clone())
        };
        let result = async {
            let dir = root.join("ssh");
            let public_key = ensure_key(&runtime_file(&runtime, "ssh-keygen"), &dir.join("id_ed25519")).await?;
            let mut status = self.start_sshd(&runtime, &dir).await?;
            status.public_key = public_key;
            status.initialization = "ready".into();
            Ok::<_, anyhow::Error>(status)
        }.await;
        match result {
            Ok(status) => { self.inner.write().await.ssh = status; Ok(()) }
            Err(error) => {
                let mut inner = self.inner.write().await;
                inner.ssh.initialization = "failed".into();
                inner.ssh.message = format!("SSH 组件准备失败：{error}");
                Err(error)
            }
        }
    }

    /// 以当前用户身份启动内置 sshd，只监听回环地址，只接受同账户设备的公钥。
    async fn start_sshd(&self, runtime: &RuntimeStatus, dir: &FsPath) -> anyhow::Result<SshStatus> {
        if let Some(port) = std::env::var("STARBRIDGE_SSH_PORT")
            .ok()
            .and_then(|value| value.parse().ok())
        {
            return Ok(SshStatus {
                port: Some(port),
                message: format!("使用外部 SSH 服务 127.0.0.1:{port}，权限未知"),
                ..SshStatus::default()
            });
        }
        let sshd = std::path::absolute(runtime_file(runtime, "sshd"))?;
        if !sshd.is_file() {
            anyhow::bail!("缺少 {}", sshd.display());
        }
        let dir = std::path::absolute(dir)?;
        let host_key = ensure_key(
            &runtime_file(runtime, "ssh-keygen"),
            &dir.join("host_ed25519"),
        )
        .await?;
        let authorized_keys = dir.join("authorized_keys");
        if !authorized_keys.is_file() {
            tokio::fs::write(&authorized_keys, "").await?;
        }
        let port = pick_port(23000, 23999)?;
        let config_path = dir.join("sshd_config");
        let sftp_server = std::path::absolute(runtime_file(runtime, "sftp-server"))?;
        tokio::fs::write(&config_path, sshd_config(&dir, &sftp_server, port)).await?;
        let log_path = dir.join("sshd.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let mut child = sshd_command(&sshd)?
            .arg("-D")
            .arg("-e")
            .arg("-f")
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| anyhow::anyhow!("启动 sshd 失败: {error}"))?;
        self.process_job.attach(&child)?;
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("sshd 启动后退出: {status}，日志见 {}", log_path.display());
        }
        *self.sshd_process.lock().await = Some(child);
        let privilege = detect_privilege();
        Ok(SshStatus {
            initialization: "ready".into(),
            privilege,
            port: Some(port),
            host_key,
            public_key: String::new(),
            login_user: current_user(),
            message: format!(
                "内置 SSH 服务已启动：127.0.0.1:{port} · {}",
                privilege_label(privilege)
            ),
        })
    }

    async fn sync_authorized_keys(&self, devices: &[DeviceInfo]) {
        let path = {
            let inner = self.inner.read().await;
            if inner.ssh.port.is_none() {
                return;
            }
            inner.root.join("ssh").join("authorized_keys")
        };
        let keys = authorized_keys(devices);
        if tokio::fs::read_to_string(&path).await.ok().as_deref() == Some(keys.as_str()) {
            return;
        }
        if let Err(error) = tokio::fs::write(&path, keys).await {
            self.inner.write().await.ssh.message = format!("更新 authorized_keys 失败：{error}");
        }
    }

    pub async fn stop_sshd(&self) {
        let mut current = self.sshd_process.lock().await;
        if let Some(child) = current.as_mut() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        *current = None;
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/", get(index))
            .route("/app.js", get(|| async { ([("content-type", "text/javascript; charset=utf-8")], CLIENT_JS) }))
            .route("/i18n.js", get(|| async { ([("content-type", "text/javascript; charset=utf-8")], CLIENT_I18N) }))
            .route("/style.css", get(|| async { ([("content-type", "text/css; charset=utf-8")], CLIENT_CSS) }))
            .route("/api/health", get(health))
            .route("/api/v1/state", get(state))
            .route("/api/v1/config", get(get_config).put(update_config))
            .route("/api/v1/login", post(login))
            .route("/api/v1/register", post(register))
            .route("/api/v1/logout", post(logout))
            .route("/api/v1/device", axum::routing::put(update_device))
            .route("/api/v1/devices", axum::routing::put(update_device))
            .route("/api/v1/devices/delete", post(delete_device))
            .route("/api/v1/devices/rebind", post(rebind_device))
            .route("/api/v1/devices/enroll", post(enrollment::resolve_enrollment))
            .route("/api/v1/connect", post(connect))
            .route("/api/v1/connect/all", post(connect_all))
            .route("/api/v1/terminate", post(terminate))
            .route("/api/v1/export", post(export_connections))
            .route("/api/v1/test/start", post(start_test))
            .route("/api/v1/test/cancel", post(cancel_test))
            .route("/api/v1/components", get(components))
            .route("/api/v1/history", post(connection_history))
            .route("/api/v1/disconnect", delete(disconnect))
            .route("/api/v1/sessions/{id}", delete(disconnect_session))
            .route("/api/v1/copy/nodes", get(copy_nodes))
            .with_state(self.clone())
    }

    pub async fn heartbeat_loop(&self) {
        let app = self.clone();
        self.spawn_background(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
            loop {
                interval.tick().await;
                {
                    let inner=app.inner.read().await;
                    if inner.auth.is_none() || inner.closing_account || inner.device_rebind_required {continue;}
                }
                let attempted = app.inner.read().await.auth_generation;
                if let Err(error) = app.heartbeat_once().await {
                    let status = registration_status(&error);
                    let unauthorized = status == Some(StatusCode::UNAUTHORIZED);
                    if app.inner.read().await.auth_generation != attempted { continue; }
                    if matches!(status, Some(StatusCode::GONE | StatusCode::FORBIDDEN | StatusCode::CONFLICT)) {
                        let _ = logout_account(&app, Some(attempted), Some("登录已失效，请重新登录以恢复这台电脑。")).await;
                        continue;
                    }
                    if unauthorized {
                        if app.refresh_auth(true).await.is_ok() { continue; }
                        app.terminate_targets(None, true).await;
                        app.sync_authorized_keys(&[]).await;
                        let mut inner = app.inner.write().await;
                        inner.auth = None; inner.auth_generation += 1;
                        clear_saved_auth(&inner.root).await;
                        inner.stage = "ready".into(); inner.last_error = Some("登录已失效，请重新登录".into());
                    } else {
                        let mut inner = app.inner.write().await;
                        if matches!(status, Some(StatusCode::CONFLICT | StatusCode::FORBIDDEN)) { inner.device_rebind_required = true; }
                        inner.stage = if matches!(status, Some(StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED)) {
                            "device_registration_failed"
                        } else { "heartbeat_failed" }.into();
                        inner.last_error = Some(error.to_string());
                    }
                }
            }
        });
    }

    fn spawn_background(&self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        let mut tasks = self
            .background_tasks
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.shutting_down.load(Ordering::Acquire) {
            return;
        }
        tasks.retain(|task| !task.is_finished());
        tasks.push(tokio::spawn(task));
    }

    /// 先取消后台工作，再回收子进程；退出不依赖中心服务器在线。
    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.diagnostic_cancel.store(true, Ordering::Release);
        // Let the diagnostic task take its bounded cleanup path before aborting work.
        for _ in 0..20 {
            if !matches!(self.inner.read().await.self_test.state.as_str(), "running" | "cleaning") { break; }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let tasks = {
            let mut tasks = self
                .background_tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            std::mem::take(&mut *tasks)
        };
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        self.stop_frpc().await;
        self.stop_sshd().await;
        // 退出先完成本地回收，中心通知最多等待一秒。
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let _operation = self.session_operations.lock().await;
            self.close_sessions(None).await;
            self.mark_offline().await;
        }).await;
    }

    async fn start_frpc_mode(
        &self,
        session: &SessionInfo,
        controller: bool,
        force_relay: bool,
    ) -> anyhow::Result<()> {
        // FRP creates its listener after the control handshake. Serialise startup and
        // reserve the port in our state before launching, rather than relying on a
        // temporarily successful bind probe while another visitor is still starting.
        let _starting = self.route_start_gate.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            anyhow::bail!("客户端正在退出");
        }
        let route = session
            .route
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("中心没有返回 FRP 路由参数"))?;
        let (root, frpc_path, ssh_port, target) = {
            let inner = self.inner.read().await;
            if inner.ended_sessions.contains(&session.session_id) || !inner.sessions.contains_key(&session.session_id) {
                anyhow::bail!("连接已终止");
            }
            (
                inner.root.clone(),
                inner.runtime.frpc_path.clone(),
                inner.ssh.port,
                inner
                    .devices
                    .iter()
                    .find(|device| device.node_id == session.target_node_id)
                    .cloned(),
            )
        };
        if !FsPath::new(&frpc_path).is_file() {
            anyhow::bail!("缺少 frpc: {frpc_path}");
        }
        let port = if controller {
            self.reserve_visitor_port(&session.session_id).await?
        } else {
            ssh_port.ok_or_else(|| anyhow::anyhow!("本机 SSH 服务尚未就绪"))?
        };
        if controller {
            let target = target.as_ref().ok_or_else(|| anyhow::anyhow!("目标设备不存在"))?;
            write_session_ssh_files(&root, &session.session_id, target, port).await?;
        }
        let config = frpc_config(route, session, controller, port, force_relay);
        let routes = root.join("routes");
        tokio::fs::create_dir_all(&routes).await?;
        let config_path = routes.join(format!("{}.toml", session.session_id));
        tokio::fs::write(&config_path, config).await?;
        let log_path = routes.join(format!("{}.log", session.session_id));
        let log = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)?;
        let mut process = async_command(&frpc_path);
        process
            .arg("-c")
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        let mut current = self.frpc_processes.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            anyhow::bail!("客户端正在退出");
        }
        {
            let inner = self.inner.read().await;
            if inner.pending_closes.contains(&session.session_id) || inner.ended_sessions.contains(&session.session_id)
                || !inner.sessions.contains_key(&session.session_id) {
                anyhow::bail!("连接已终止");
            }
        }
        if let Some(mut old) = current.remove(&session.session_id) {
            let _ = old.kill().await;
            let _ = old.wait().await;
        }
        if self.shutting_down.load(Ordering::Acquire) {
            anyhow::bail!("客户端正在退出");
        }
        let child = process
            .spawn()
            .map_err(|error| anyhow::anyhow!("启动 frpc 失败: {error}"))?;
        self.process_job.attach(&child)?;
        // 在首次 await 前登记，后台任务被取消时 shutdown 仍能等待进程退出。
        current.insert(session.session_id.clone(), child);
        drop(current);
        let started = tokio::time::Instant::now();
        loop {
            // Never hold the child table over a network wait: termination must still
            // kill this visitor immediately, including during the FRPS handshake.
            {
                let mut current = self.frpc_processes.lock().await;
                let child = current.get_mut(&session.session_id).ok_or_else(|| anyhow::anyhow!("连接已终止"))?;
                if let Some(status) = child.try_wait()? {
                    current.remove(&session.session_id);
                    anyhow::bail!("frpc 启动后退出: {status}，日志见 {}", log_path.display());
                }
            }
            let log = tokio::fs::read_to_string(&log_path).await.unwrap_or_default();
            if let Some(error) = visitor_start_failure(&log) {
                anyhow::bail!("本地访问端口 {port} 启动失败：{error}");
            }
            if controller && log.contains("start visitor success") { break; }
            if !controller && started.elapsed() >= std::time::Duration::from_millis(700) { break; }
            if started.elapsed() >= std::time::Duration::from_secs(10) {
                anyhow::bail!("本地访问端口 {port} 未确认监听就绪，请检查连接服务和安全软件记录");
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let mut inner = self.inner.write().await;
        if inner.pending_closes.contains(&session.session_id) || inner.ended_sessions.contains(&session.session_id)
            || !inner.sessions.contains_key(&session.session_id) {
            drop(inner); self.stop_session(&session.session_id).await;
            anyhow::bail!("连接已终止");
        }
        *inner.route_generations.entry(session.session_id.clone()).or_default() += 1;
        if controller {
            inner.visitor_ports.insert(session.session_id.clone(), port);
        }
        Ok(())
    }

    async fn reserve_visitor_port(&self, session_id: &str) -> anyhow::Result<u16> {
        let mut inner = self.inner.write().await;
        if let Some(port) = inner.visitor_ports.get(session_id) { return Ok(*port); }
        for port in 22000..=22999 {
            if !inner.visitor_ports.values().any(|reserved| *reserved == port)
                && std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
            {
                inner.visitor_ports.insert(session_id.into(), port);
                return Ok(port);
            }
        }
        anyhow::bail!("本地访问端口已耗尽");
    }

    pub async fn stop_frpc(&self) {
        let mut current = self.frpc_processes.lock().await;
        for (_, mut child) in current.drain() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    async fn stop_session(&self, id: &str) {
        if let Some(mut child) = self.frpc_processes.lock().await.remove(id) {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        let mut inner = self.inner.write().await;
        inner.sessions.remove(id);
        inner.ended_sessions.insert(id.into());
        inner.verified_sessions.remove(id);
        inner.visitor_ports.remove(id);
        inner.route_generations.remove(id);
    }

    /// 先清理本地，再尽力通知中心；网络中断不能阻止用户停止连接。
    async fn close_sessions(&self, only: Option<&str>) {
        let ids = {
            let mut inner = self.inner.write().await;
            let ids = inner.sessions.keys().filter(|id| only.is_none_or(|only| only == id.as_str())).cloned().collect::<Vec<_>>();
            inner.pending_closes.extend(ids.iter().cloned());
            if let Err(error) = save_json(&inner.root.join("pending-closes.json"), &inner.pending_closes).await {
                inner.last_error = Some(format!("保存断开状态失败：{error}"));
            }
            ids
        };
        for id in &ids { self.stop_session(id).await; }
        self.retry_pending_closes().await;
        let mut inner = self.inner.write().await;
        inner.stage = if inner.auth.is_some() { "online" } else { "ready" }.into();
    }

    async fn retry_pending_closes(&self) {
        let (auth, url, identity, ids) = {
            let inner = self.inner.read().await;
            (inner.auth.clone(), inner.config.server_url.clone(), inner.identity.clone(), inner.pending_closes.iter().cloned().collect::<Vec<_>>())
        };
        if ids.is_empty() { return; }
        let mut failure = None;
        if let Some(auth) = auth {
            for id in ids {
                match self.http.delete(format!("{url}/api/v1/sessions/{id}"))
                    .bearer_auth(&auth.access_token)
                    .header("x-starbridge-node-id", &identity.node_id)
                    .header("x-starbridge-device-token", &identity.device_token)
                    .send().await.and_then(reqwest::Response::error_for_status) {
                    Ok(_) => { self.inner.write().await.pending_closes.remove(&id); }
                    Err(error) => failure = Some(format!("本地连接已断开，中心状态同步失败：{error}")),
                }
            }
        }
        let mut inner = self.inner.write().await;
        if let Err(error) = save_json(&inner.root.join("pending-closes.json"), &inner.pending_closes).await {
            failure = Some(format!("保存断开状态失败：{error}"));
        }
        inner.last_error = failure;
    }

    async fn mark_offline(&self) {
        let (auth, url, identity) = {
            let inner = self.inner.read().await;
            (inner.auth.clone(), inner.config.server_url.clone(), inner.identity.clone())
        };
        if let Some(auth) = auth {
            let _ = self.http.post(format!("{url}/api/v1/devices/heartbeat"))
                .bearer_auth(&auth.access_token)
                .json(&HeartbeatRequest {
                    node_id: identity.node_id,
                    device_token: identity.device_token,
                    status: "offline".into(),
                }).send().await;
        }
    }
}

fn device_registration(inner: &Inner) -> DeviceRegistration {
    let mut capabilities = Vec::new();
    if inner.runtime.ssh_available && runtime_file(&inner.runtime, "ssh-keygen").is_file() {
        capabilities.push("ssh_client".into());
    }
    if runtime_file(&inner.runtime, "sshd").is_file() || std::env::var_os("STARBRIDGE_SSH_PORT").is_some() {
        capabilities.extend(["ssh_server", "ssh", "sftp", "scp"].map(String::from));
    }
    DeviceRegistration {
        node_id: inner.identity.node_id.clone(),
        device_token: inner.identity.pending_rebind_token.as_ref().unwrap_or(&inner.identity.device_token).clone(),
        machine_key: Some(inner.identity.machine_key.clone()),
        display_name: inner.config.display_name.clone(),
        purpose: inner.config.purpose.clone(),
        role: Role::Peer,
        ssh_alias: inner.config.ssh_alias.clone(),
        ssh_user: if inner.ssh.login_user.is_empty() { inner.config.ssh_user.clone() } else { inner.ssh.login_user.clone() },
        ssh_privilege: inner.ssh.privilege,
        ssh_public_key: inner.ssh.public_key.clone(),
        ssh_host_key: inner.ssh.host_key.clone(),
        capabilities,
        allow_relay: inner.config.allow_relay,
        direct_only: inner.config.direct_only,
        version: env!("CARGO_PKG_VERSION").into(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        hostname: hostname(),
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
    if !inner.config.remember_login { clear_saved_auth(&inner.root).await; }
    Ok(Json(inner.config.clone()))
}

async fn disconnect(State(app): State<ClientApp>) -> Result<Json<ClientSnapshot>, ApiError> {
    app.terminate_targets(None, true).await;
    Ok(Json(snapshot(&*app.inner.read().await)))
}

async fn disconnect_session(State(app): State<ClientApp>, Path(id): Path<String>)
    -> Result<Json<ClientSnapshot>, ApiError>
{
    let peer = {
        let inner = app.inner.read().await;
        inner.sessions.get(&id).map(|session| if session.initiator_node_id == inner.identity.node_id {
            session.target_node_id.clone()
        } else { session.initiator_node_id.clone() })
    };
    if let Some(peer) = peer { app.terminate_targets(Some(&peer), false).await; }
    Ok(Json(snapshot(&*app.inner.read().await)))
}

async fn copy_nodes(State(app): State<ClientApp>) -> Json<serde_json::Value> {
    let inner = app.inner.read().await;
    let text = inner
        .devices
        .iter()
        .filter(|d| d.node_id != inner.identity.node_id && d.online && d.capabilities.iter().any(|capability| capability == "ssh_server"))
        .map(|d| {
            format!(
                "{} · {} · {} · 在线",
                d.display_name,
                if d.purpose.is_empty() { "未设置用途" } else { &d.purpose },
                d.os,
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
    let (auth, url, identity) = {
        let inner = app.inner.read().await;
        (
            inner.auth.clone().ok_or_else(|| anyhow::anyhow!("未登录"))?,
            inner.config.server_url.clone(),
            inner.identity.clone(),
        )
    };
    Ok(app.http
        .post(format!("{url}/api/v1/sessions/{}/status", info.session_id))
        .bearer_auth(auth.access_token)
        .header("x-starbridge-node-id", identity.node_id)
        .header("x-starbridge-device-token", identity.device_token)
        .json(&SessionStatusRequest { status, path, failure_reason })
        .send().await?.error_for_status()?.json::<SessionInfo>().await?)
}

fn snapshot(inner: &Inner) -> ClientSnapshot {
    let mut sessions: Vec<_> = inner.sessions.values().map(|session| SessionView {
        session_id: session.session_id.clone(),
        initiator_node_id: session.initiator_node_id.clone(),
        target_node_id: session.target_node_id.clone(),
        policy: session.policy,
        path: session.path,
        status: session.status,
        created_at: session.created_at,
        expires_at: session.expires_at,
        failure_reason: session.failure_reason.clone(),
        ssh_command: session_ssh_command(inner, session),
        ssh_ready: session.status == SessionStatus::Connected && inner.verified_sessions.contains_key(&session.session_id),
        verified_at: inner.verified_sessions.get(&session.session_id).copied(),
    }).collect();
    sessions.sort_by(|a, b| (a.created_at, &a.session_id).cmp(&(b.created_at, &b.session_id)));
    // 旧 API 字段仍指向最近的发起连接，避免已有联调脚本失效。
    let primary = sessions.iter().rev().find(|session| session.initiator_node_id == inner.identity.node_id)
        .or_else(|| sessions.last()).cloned();
    ClientSnapshot {
        role: inner.role,
        logged_in: inner.auth.is_some(),
        username: inner.auth.as_ref().map(|a| a.username.clone()),
        server_url: inner.config.server_url.clone(),
        node_id: inner.identity.node_id.clone(),
        instance_id: inner.instance_id.clone(),
        display_name: inner.config.display_name.clone(),
        devices: inner.devices.clone(),
        ssh_command: primary.as_ref().and_then(|session| session.ssh_command.clone()),
        session: primary,
        sessions,
        stage: inner.stage.clone(),
        last_error: inner.last_error.clone(),
        device_rebind_required: inner.device_rebind_required,
        enrollment_candidates: inner.enrollment_candidates.clone(),
        runtime: inner.runtime.clone(),
        ssh: inner.ssh.clone(),
        updated_at: now_unix(),
        remember_login: inner.config.remember_login,
        last_username: inner.config.last_username.clone(),
        purpose: inner.config.purpose.clone(),
        platform: std::env::consts::OS.into(),
        device_operations: inner.operations.values().cloned().collect(),
        batch: batch_view(inner),
        self_test: inner.self_test.clone(),
        allow_relay: inner.config.allow_relay,
        direct_only: inner.config.direct_only,
    }
}

fn session_ssh_command(inner: &Inner, session: &SessionInfo) -> Option<String> {
    if session.initiator_node_id != inner.identity.node_id { return None; }
    if session.status != SessionStatus::Connected || !inner.verified_sessions.contains_key(&session.session_id) { return None; }
    inner.visitor_ports.get(&session.session_id)?;
    let target = inner.devices.iter().find(|device| device.node_id == session.target_node_id)?;
    let config = std::path::absolute(inner.root.join("ssh").join("sessions").join(&session.session_id).join("config")).ok()?;
    if inner.runtime.ssh_from_system {
        let config = config.display().to_string();
        let config = if cfg!(windows) {
            config.replace('`', "``").replace('$', "`$").replace('"', "`\"")
        } else {
            config.replace('\\', "\\\\").replace('$', "\\$").replace('`', "\\`").replace('"', "\\\"")
        };
        let alias = if target.ssh_alias.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            target.ssh_alias.clone()
        } else if cfg!(windows) {
            format!("'{}'", target.ssh_alias.replace('\'', "''"))
        } else {
            format!("'{}'", target.ssh_alias.replace('\'', "'\"'\"'"))
        };
        return Some(format!("ssh -F \"{config}\" {alias}"));
    }
    let executable = std::path::absolute(&inner.runtime.ssh_path).ok()?.display().to_string();
    if cfg!(windows) {
        Some(format!("& '{}' -F '{}' '{}'", executable.replace('\'', "''"),
            config.display().to_string().replace('\'', "''"), target.ssh_alias.replace('\'', "''")))
    } else {
        Some(format!("'{}' -F '{}' '{}'", executable.replace('\'', "'\"'\"'"),
            config.display().to_string().replace('\'', "'\"'\"'"), target.ssh_alias.replace('\'', "'\"'\"'")))
    }
}

async fn runtime_status(root: &FsPath) -> RuntimeStatus {
    let executable = std::env::current_exe().ok();
    let runtime = runtime_directory(root, executable.as_deref().and_then(FsPath::parent));
    let frpc = runtime.join(if cfg!(windows) { "frpc.exe" } else { "frpc" });
    let bundled_ssh = runtime.join(if cfg!(windows) { "ssh.exe" } else { "ssh" });
    let frpc_available = frpc.is_file();
    let system_ssh = command_path("ssh");
    let (ssh, ssh_available) = select_ssh_client(system_ssh.clone(), bundled_ssh, ssh_client_available).await;
    let ssh_from_system = ssh_available && system_ssh.as_ref() == Some(&ssh);
    RuntimeStatus {
        frpc_path: frpc.display().to_string(),
        frpc_available,
        ssh_path: ssh.display().to_string(),
        ssh_available,
        ssh_from_system,
        message: if frpc_available {
            "FRP 运行时已找到".into()
        } else {
            "缺少 frpc，连接只能验证控制面".into()
        },
    }
}

async fn select_ssh_client<F, Fut>(system: Option<PathBuf>, bundled: PathBuf, mut probe: F) -> (PathBuf, bool)
where
    F: FnMut(PathBuf) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    if let Some(system) = system {
        if probe(system.clone()).await { return (system, true); }
    }
    let available = probe(bundled.clone()).await;
    (bundled, available)
}

async fn ssh_client_available(executable: PathBuf) -> bool {
    // 实际启动而非只检查文件存在；超时后 kill_on_drop 会终止检测进程。
    matches!(tokio::time::timeout(std::time::Duration::from_secs(3),
        async_command(executable).arg("-V").stdin(Stdio::null())
            .stdout(Stdio::null()).stderr(Stdio::null()).status()
    ).await, Ok(Ok(status)) if status.success())
}

fn runtime_directory(root: &FsPath, executable_dir: Option<&FsPath>) -> PathBuf {
    let local_runtime = root.join("runtime");
    let bundle_runtime = executable_dir
        .filter(|directory| directory.file_name().is_some_and(|name| name == "MacOS"))
        .and_then(FsPath::parent)
        .filter(|directory| directory.file_name().is_some_and(|name| name == "Contents"))
        .map(|directory| directory.join("Resources/runtime"));
    std::iter::once(local_runtime.clone())
        .chain(root.parent().map(|parent| parent.join("runtime")))
        .chain(executable_dir.map(|directory| directory.join("runtime")))
        .chain(bundle_runtime)
        .find(|directory| directory.is_dir())
        .unwrap_or(local_runtime)
}

fn quiet_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let command = std::process::Command::new(program);
    #[cfg(windows)]
    let command = {
        use std::os::windows::process::CommandExt;
        let mut command = command;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        command
    };
    command
}

fn async_command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut command = tokio::process::Command::from(quiet_command(program));
    command.kill_on_drop(true);
    command
}

fn sshd_command(program: &FsPath) -> anyhow::Result<tokio::process::Command> {
    let command = async_command(program);
    #[cfg(windows)]
    let command = {
        let mut command = command;
        let launcher = sshd_session_launcher()?;
        anyhow::ensure!(launcher.is_file(), "缺少 SSH 会话启动器 {}，请完整构建或解压客户端", launcher.display());
        command.env("STARBRIDGE_SSHD_SESSION_PATH",
            std::path::absolute(program.with_file_name("sshd-session.exe"))?);
        command
    };
    Ok(command)
}

#[cfg(windows)]
fn sshd_session_launcher() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_exe()?.with_file_name("starbridge-ssh-session.exe"))
}

fn pick_port(start: u16, end: u16) -> anyhow::Result<u16> {
    for port in start..=end {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!("本地 visitor 端口范围 {start}-{end} 已耗尽")
}

fn visitor_start_failure(log: &str) -> Option<&str> {
    log.lines().find_map(|line| line.split_once("start error:").map(|(_, error)| error.trim()))
}

fn frpc_config(
    route: &FrpRoute,
    session: &SessionInfo,
    controller: bool,
    local_port: u16,
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
            local_port
        ));
    } else {
        config.push_str(&format!(
            "[[proxies]]\nname = \"{}\"\ntype = \"{}\"\nsecretKey = \"{}\"\nlocalIP = \"127.0.0.1\"\nlocalPort = {}\n",
            route.proxy_name,
            mode,
            route.secret_key,
            local_port
        ));
    }
    config
}

fn runtime_file(runtime: &RuntimeStatus, name: &str) -> PathBuf {
    if name == "ssh" {
        return PathBuf::from(&runtime.ssh_path);
    }
    let dir = FsPath::new(&runtime.frpc_path)
        .parent()
        .map(FsPath::to_path_buf)
        .unwrap_or_default();
    dir.join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    })
}

/// 生成或读取 ed25519 密钥，返回单行公钥。
async fn ensure_key(keygen: &FsPath, path: &FsPath) -> anyhow::Result<String> {
    let public = path.with_extension("pub");
    if !path.is_file() || !public.is_file() {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let _ = tokio::fs::remove_file(path).await;
        let _ = tokio::fs::remove_file(&public).await;
        let output = async_command(keygen)
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "starbridge", "-f"])
            .arg(path)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|error| anyhow::anyhow!("无法启动 {}: {error}", keygen.display()))?;
        if !output.status.success() {
            anyhow::bail!(
                "ssh-keygen 退出码 {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }
    Ok(tokio::fs::read_to_string(&public).await?.trim().to_string())
}

fn ssh_path(path: &FsPath) -> String {
    format!("\"{}\"", path.display().to_string().replace('\\', "/"))
}

fn sshd_config(dir: &FsPath, sftp_server: &FsPath, port: u16) -> String {
    let mut config = format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nPubkeyAuthentication yes\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nStrictModes no\nSubsystem sftp {}\n",
        ssh_path(&dir.join("host_ed25519")),
        ssh_path(&dir.join("sshd.pid")),
        ssh_path(&dir.join("authorized_keys")),
        ssh_path(sftp_server),
    );
    #[cfg(windows)]
    if let Ok(launcher) = sshd_session_launcher() {
        config.push_str(&format!("SshdSessionPath {}\n", ssh_path(&launcher)));
    }
    // 源码构建的 sshd 把 sshd-session/sshd-auth 的路径编译成安装前缀，需要改指运行时目录。
    if !cfg!(windows) {
        let runtime = sftp_server.parent().unwrap_or(FsPath::new(""));
        let session = runtime.join("sshd-session");
        if session.is_file() {
            config.push_str(&format!("SshdSessionPath {}\n", ssh_path(&session)));
        }
        let auth = runtime.join("sshd-auth");
        if auth.is_file() {
            config.push_str(&format!("SshdAuthPath {}\n", ssh_path(&auth)));
        }
    }
    config
}

/// 同账户设备的公钥，每行一个。
fn authorized_keys(devices: &[DeviceInfo]) -> String {
    devices
        .iter()
        .filter(|device| !device.ssh_public_key.is_empty())
        .map(|device| format!("{}\n", device.ssh_public_key))
        .collect()
}

fn ssh_client_config(root: &FsPath, dir: &FsPath, target: &DeviceInfo, port: u16) -> String {
    format!(
        "Host {}\n  HostName 127.0.0.1\n  Port {port}\n  User {}\n  IdentityFile {}\n  IdentitiesOnly yes\n  HostKeyAlias {}\n  UserKnownHostsFile {}\n  StrictHostKeyChecking {}\n",
        target.ssh_alias,
        target.ssh_user,
        ssh_path(&root.join("ssh").join("id_ed25519")),
        target.ssh_alias,
        ssh_path(&dir.join("known_hosts")),
        if target.ssh_host_key.is_empty() {
            "accept-new"
        } else {
            "yes"
        },
    )
}

async fn write_session_ssh_files(
    root: &FsPath,
    session_id: &str,
    target: &DeviceInfo,
    port: u16,
) -> anyhow::Result<()> {
    let root = std::path::absolute(root)?;
    let dir = root.join("ssh").join("sessions").join(session_id);
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(dir.join("config"), ssh_client_config(&root, &dir, target, port)).await?;
    if !target.ssh_host_key.is_empty() {
        tokio::fs::write(
            dir.join("known_hosts"),
            format!("{} {}\n", target.ssh_alias, target.ssh_host_key),
        )
        .await?;
    }
    Ok(())
}

/// 检测当前进程权限；sshd 由本进程拉起，权限相同。
fn detect_privilege() -> SshPrivilege {
    if cfg!(windows) {
        let Ok(output) = quiet_command("whoami")
            .args(["/groups", "/fo", "csv", "/nh"])
            .output()
        else {
            return SshPrivilege::Unknown;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        if text.contains("S-1-16-16384") {
            SshPrivilege::System
        } else if text.contains("S-1-16-12288") {
            SshPrivilege::Admin
        } else if text.contains("S-1-16-8192") {
            SshPrivilege::User
        } else {
            SshPrivilege::Unknown
        }
    } else {
        match quiet_command("id").arg("-u").output() {
            Ok(output) if String::from_utf8_lossy(&output.stdout).trim() == "0" => {
                SshPrivilege::Admin
            }
            Ok(output) if output.status.success() => SshPrivilege::User,
            _ => SshPrivilege::Unknown,
        }
    }
}

fn privilege_label(privilege: SshPrivilege) -> &'static str {
    match privilege {
        SshPrivilege::User => "普通用户权限，执行可能受限",
        SshPrivilege::Admin => "管理员权限",
        SshPrivilege::System => "SYSTEM 权限",
        SshPrivilege::Unknown => "权限未知",
    }
}

fn command_path(name: &str) -> Option<PathBuf> {
    let directories: Vec<_> = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .collect();
    directories.into_iter().find_map(|dir| {
        let path = dir.join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.into()
        });
        path.is_file().then(|| std::path::absolute(path).ok()).flatten()
    })
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

fn current_user() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".into())
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(system_hostname)
        .unwrap_or_else(|| "starbridge-node".into())
}

/// macOS 和多数 Linux 默认不导出 HOSTNAME 环境变量，向系统查询主机名。
fn system_hostname() -> Option<String> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = quiet_command("scutil");
        command.args(["--get", "LocalHostName"]);
        command
    } else {
        quiet_command("hostname")
    };
    let output = command
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
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

#[derive(Debug)]
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

    #[tokio::test]
    async fn corrupt_configuration_reports_a_translatable_error_without_changing_credentials() {
        let root = std::env::temp_dir().join(format!("starbridge-config-error-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        for name in ["config.json", "identity.secure", "auth.secure"] {
            std::fs::write(root.join(name), b"preserve original test contents").unwrap();
        }
        let error = match ClientApp::load(Role::Peer, root.clone()).await {
            Ok(_) => panic!("corrupt configuration must block startup"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "本机配置无法读取，请先检查配置后重试");
        assert_eq!(profile::kind(&error), profile::FailureKind::Storage);
        assert!(error.downcast_ref::<serde_json::Error>().is_some());
        for name in ["config.json", "identity.secure", "auth.secure"] {
            assert_eq!(std::fs::read(root.join(name)).unwrap(), b"preserve original test contents");
        }
        assert!(!root.join("recovery-backups").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn usable_system_ssh_is_preferred_without_probing_bundle() {
        let system = PathBuf::from("system/ssh");
        let (selected, available) = select_ssh_client(Some(system.clone()), "bundle/ssh".into(), |path| {
            assert_eq!(path, system);
            std::future::ready(true)
        }).await;
        assert_eq!(selected, system);
        assert!(available);
    }

    #[tokio::test]
    async fn missing_or_broken_system_ssh_falls_back_to_bundle() {
        let bundled = PathBuf::from("bundle/ssh");
        for system in [None, Some(PathBuf::from("broken/ssh"))] {
            let mut probed = Vec::new();
            let (selected, available) = select_ssh_client(system.clone(), bundled.clone(), |path| {
                let works = path == bundled;
                probed.push(path);
                std::future::ready(works)
            }).await;
            assert_eq!(selected, bundled);
            assert!(available);
            assert_eq!(probed, system.into_iter().chain([bundled.clone()]).collect::<Vec<_>>());
        }
    }

    #[tokio::test]
    async fn unavailable_ssh_clients_are_not_reported_as_ready() {
        let bundled = PathBuf::from("bundle/ssh");
        let (selected, available) = select_ssh_client(Some("broken/ssh".into()), bundled.clone(), |_| {
            std::future::ready(false)
        }).await;
        assert_eq!(selected, bundled);
        assert!(!available);

        let missing = std::env::temp_dir().join(format!("starbridge-missing-{}", uuid::Uuid::new_v4()));
        assert!(!ssh_client_available(missing).await);
        // 测试程序不接受 -V，会以非零状态退出。
        assert!(!ssh_client_available(std::env::current_exe().unwrap()).await);
    }

    #[test]
    fn portable_runtime_preserves_instance_and_shared_overrides() {
        let temporary = std::env::temp_dir()
            .join(format!("starbridge-runtime-{}", uuid::Uuid::new_v4()));
        let executable_dir = temporary.join("portable");
        let root = temporary.join("data").join("controller");
        let portable = executable_dir.join("runtime");
        let shared = root.parent().unwrap().join("runtime");
        let local = root.join("runtime");
        assert_eq!(runtime_directory(&root, Some(&executable_dir)), local);
        std::fs::create_dir_all(&portable).unwrap();
        assert_eq!(runtime_directory(&root, Some(&executable_dir)), portable);
        std::fs::create_dir_all(&shared).unwrap();
        assert_eq!(runtime_directory(&root, Some(&executable_dir)), shared);
        std::fs::create_dir_all(&local).unwrap();
        assert_eq!(runtime_directory(&root, Some(&executable_dir)), local);
        std::fs::remove_dir_all(&temporary).unwrap();
    }

    #[test]
    fn app_bundle_runtime_is_found_with_external_user_data() {
        let temporary = std::env::temp_dir()
            .join(format!("starbridge-bundle-{}", uuid::Uuid::new_v4()));
        let executable_dir = temporary.join("StarBridge.app/Contents/MacOS");
        let runtime = temporary.join("StarBridge.app/Contents/Resources/runtime");
        let root = temporary.join("user-data/node");
        std::fs::create_dir_all(&runtime).unwrap();
        assert_eq!(runtime_directory(&root, Some(&executable_dir)), runtime);
        let override_dir = root.join("runtime");
        std::fs::create_dir_all(&override_dir).unwrap();
        assert_eq!(runtime_directory(&root, Some(&executable_dir)), override_dir);
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_background_work_and_rejects_restart() {
        let root = std::env::temp_dir()
            .join(format!("starbridge-shutdown-{}", uuid::Uuid::new_v4()));
        let app = ClientApp::load(Role::Controller, root.clone()).await.unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        app.spawn_background(async move {
            let _sender = sender;
            std::future::pending::<()>().await;
        });
        app.heartbeat_loop().await;
        tokio::time::timeout(std::time::Duration::from_secs(2), app.shutdown())
            .await
            .expect("offline shutdown should not wait for the server");
        assert!(receiver.await.is_err(), "background future must be dropped");
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        app.spawn_background(async move {
            let _sender = sender;
            std::future::pending::<()>().await;
        });
        assert!(
            receiver.await.is_err(),
            "shutdown must prevent new background work"
        );
        let error = app
            .start_frpc_mode(&session(ConnectionPolicy::RelayOnly), true, false)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("正在退出"));
        assert!(app.frpc_processes.lock().await.is_empty());
        assert!(app.sshd_process.lock().await.is_none());
        app.shutdown().await;
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn session(policy: ConnectionPolicy) -> SessionInfo {
        SessionInfo {
            session_id: "session-id".into(),
            initiator_node_id: "controller".into(),
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
        let config = frpc_config(info.route.as_ref().unwrap(), &info, false, 23000, false);
        assert!(config.contains("type = \"xtcp\""));
        assert!(config.contains("localPort = 23000"));
    }

    fn device(role: Role, public_key: &str) -> DeviceInfo {
        DeviceInfo {
            node_id: "node".into(),
            account: "alice".into(),
            display_name: "node".into(),
            purpose: String::new(), occupancy: "idle".into(), occupied_by: None, outgoing_count: 0,
            role,
            ssh_alias: "node-alias".into(),
            ssh_user: "alice".into(),
            ssh_privilege: SshPrivilege::User,
            ssh_public_key: public_key.into(),
            ssh_host_key: String::new(),
            capabilities: vec!["ssh".into()],
            allow_relay: true,
            direct_only: false,
            version: "test".into(),
            os: "windows".into(),
            arch: "x86_64".into(),
            hostname: "node".into(),
            online: true,
            last_heartbeat: 0,
        }
    }

    #[tokio::test]
    async fn loading_profile_does_not_start_connection_components() {
        let root = std::env::temp_dir().join(format!("starbridge-lazy-{}", uuid::Uuid::new_v4()));
        let app = ClientApp::load(Role::Peer, root.clone()).await.unwrap();
        assert_eq!(app.inner.read().await.ssh.initialization, "not_initialized");
        assert!(!root.join("ssh").exists());
        assert!(app.sshd_process.lock().await.is_none());
        app.shutdown().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn concurrent_visitors_reserve_unique_ports_before_any_listener_starts() {
        let root = std::env::temp_dir().join(format!("starbridge-ports-{}", uuid::Uuid::new_v4()));
        let app = ClientApp::load(Role::Peer, root.clone()).await.unwrap();
        let reserved = Arc::new(tokio::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for index in 0..3 {
            let app = app.clone(); let reserved = reserved.clone();
            workers.push(tokio::spawn(async move {
                let id = format!("concurrent-{index}");
                let port = app.reserve_visitor_port(&id).await.unwrap();
                // Reproduce slow FRP startup: all three requests allocate before any
                // socket starts listening, so an OS-only availability probe is insufficient.
                reserved.wait().await;
                let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
                assert_eq!(app.reserve_visitor_port(&id).await.unwrap(), port);
                (port, listener)
            }));
        }
        let mut listeners = Vec::new();
        for worker in workers { listeners.push(worker.await.unwrap()); }
        assert_eq!(listeners.iter().map(|(port, _)| port).collect::<HashSet<_>>().len(), 3);
        assert_eq!(app.inner.read().await.visitor_ports.len(), 3);
        drop(listeners);
        app.shutdown().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn unverified_or_terminated_sessions_cannot_export_commands() {
        let root = std::env::temp_dir().join(format!("starbridge-export-{}", uuid::Uuid::new_v4()));
        let app = ClientApp::load(Role::Peer, root.clone()).await.unwrap();
        let mut target = device(Role::Peer, ""); target.node_id = "target".into();
        let mut info = session(ConnectionPolicy::RelayOnly);
        {
            let mut inner = app.inner.write().await;
            info.initiator_node_id = inner.identity.node_id.clone(); info.target_node_id = target.node_id.clone();
            info.status = SessionStatus::Connected;
            inner.devices.push(target); inner.sessions.insert(info.session_id.clone(),info.clone());
            inner.visitor_ports.insert(info.session_id.clone(),22000);
            assert!(session_ssh_command(&inner,&info).is_none());
            inner.verified_sessions.insert(info.session_id.clone(),now_unix());
            let config = std::path::absolute(root.join("ssh/sessions").join(&info.session_id).join("config")).unwrap();
            inner.runtime.ssh_from_system = true;
            assert_eq!(session_ssh_command(&inner,&info).unwrap(),
                format!("ssh -F \"{}\" node-alias", config.display()));
            inner.runtime.ssh_from_system = false;
            inner.runtime.ssh_path = root.join("bundled runtime/ssh").display().to_string();
            let executable = std::path::absolute(&inner.runtime.ssh_path).unwrap();
            let expected = if cfg!(windows) {
                format!("& '{}' -F '{}' 'node-alias'", executable.display(), config.display())
            } else {
                format!("'{}' -F '{}' 'node-alias'", executable.display(), config.display())
            };
            assert_eq!(session_ssh_command(&inner,&info).unwrap(), expected);
        }
        let ready_export = export_connections(State(app.clone()),Json(ExportForm {target_node_ids:vec![]})).await.unwrap().0;
        assert!(!ready_export["text"].as_str().unwrap().contains("节点 ID"));
        assert!(ready_export["items"][0].get("node_id").is_none());
        assert!(ready_export["items"][0].get("source_node_id").is_none());
        app.terminate_targets(None,false).await;
        let exported = export_connections(State(app.clone()),Json(ExportForm {target_node_ids:vec![]})).await.unwrap().0;
        assert_eq!(exported["items"].as_array().unwrap().len(),0);
        assert_eq!(exported["excluded"].as_array().unwrap().len(),1);
        app.shutdown().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn authorized_keys_trust_same_account_devices_regardless_of_legacy_role() {
        let keys = authorized_keys(&[
            device(Role::Controller, "ssh-ed25519 AAAA controller"),
            device(Role::Executor, "ssh-ed25519 BBBB executor"),
            device(Role::Controller, ""),
        ]);
        assert_eq!(keys, "ssh-ed25519 AAAA controller\nssh-ed25519 BBBB executor\n");
    }

    #[test]
    fn sshd_config_listens_on_loopback_with_keys_only() {
        let config = sshd_config(
            FsPath::new("D:\\sb\\ssh"),
            FsPath::new("D:\\rt\\sftp-server.exe"),
            23000,
        );
        assert!(config.contains("Port 23000\nListenAddress 127.0.0.1\n"));
        assert!(config.contains("PasswordAuthentication no"));
        assert!(config.contains("AuthorizedKeysFile \"D:/sb/ssh/authorized_keys\""));
    }

    #[test]
    fn ssh_client_config_pins_host_key_when_known() {
        let mut target = device(Role::Executor, "");
        assert!(
            ssh_client_config(FsPath::new("root"), FsPath::new("root/ssh"), &target, 22000)
                .contains("StrictHostKeyChecking accept-new")
        );
        target.ssh_host_key = "ssh-ed25519 CCCC".into();
        let config = ssh_client_config(FsPath::new("root"), FsPath::new("root/ssh"), &target, 22000);
        assert!(
            config.contains("Host node-alias\n  HostName 127.0.0.1\n  Port 22000\n  User alice\n")
        );
        assert!(config.contains("StrictHostKeyChecking yes"));
    }

    #[tokio::test]
    async fn simultaneous_sessions_keep_separate_ssh_configs_and_direction() {
        let root = std::env::temp_dir().join(format!("starbridge-peer-{}", uuid::Uuid::new_v4()));
        let app = ClientApp::load(Role::Peer, root.clone()).await.unwrap();
        let mut target = device(Role::Peer, "ssh-ed25519 peer");
        target.ssh_host_key = "ssh-ed25519 target-host".into();
        let mut outgoing = session(ConnectionPolicy::RelayOnly);
        let mut incoming = outgoing.clone();
        incoming.session_id = "incoming".into();
        {
            let mut inner = app.inner.write().await;
            outgoing.initiator_node_id = inner.identity.node_id.clone();
            outgoing.target_node_id = target.node_id.clone();
            incoming.initiator_node_id = target.node_id.clone();
            incoming.target_node_id = inner.identity.node_id.clone();
            inner.devices.push(target.clone());
            inner.sessions.insert(outgoing.session_id.clone(), outgoing.clone());
            inner.sessions.insert(incoming.session_id.clone(), incoming.clone());
            inner.visitor_ports.insert(outgoing.session_id.clone(), 22001);
            inner.sessions.get_mut(&outgoing.session_id).unwrap().status = SessionStatus::Connected;
            inner.verified_sessions.insert(outgoing.session_id.clone(), now_unix());
            let state = snapshot(&inner);
            assert_eq!(state.sessions.len(), 2);
            assert_eq!(state.sessions.iter().filter(|session| session.ssh_command.is_some()).count(), 1);
            assert_eq!(state.session.unwrap().session_id, outgoing.session_id);
        }
        write_session_ssh_files(&root, &outgoing.session_id, &target, 22001).await.unwrap();
        write_session_ssh_files(&root, "another-target", &target, 22002).await.unwrap();
        let config = tokio::fs::read_to_string(root.join("ssh/sessions/session-id/config")).await.unwrap();
        let other = tokio::fs::read_to_string(root.join("ssh/sessions/another-target/config")).await.unwrap();
        assert!(config.contains("Port 22001"));
        assert!(other.contains("Port 22002"));
        assert!(config.contains(&ssh_path(&root.join("ssh/id_ed25519"))));
        assert!(config.contains("sessions/session-id/known_hosts"));
        assert!(other.contains("sessions/another-target/known_hosts"));
        // 中心不可达也必须清理选中的连接，并保留另一个方向。
        {
            let mut inner = app.inner.write().await;
            inner.config.server_url = "http://127.0.0.1:1".into();
            inner.auth = Some(AuthResponse { access_token: "test".into(), username: "alice".into(), expires_at: u64::MAX, refresh_token: None, refresh_expires_at: None });
        }
        app.close_sessions(Some(&outgoing.session_id)).await;
        let inner = app.inner.read().await;
        assert!(!inner.sessions.contains_key(&outgoing.session_id));
        assert!(inner.sessions.contains_key(&incoming.session_id));
        assert!(inner.visitor_ports.is_empty());
        assert!(inner.pending_closes.contains(&outgoing.session_id));
        assert!(inner.last_error.as_ref().unwrap().contains("本地连接已断开"));
        drop(inner);
        let pending: HashSet<String> = serde_json::from_slice(
            &tokio::fs::read(root.join("pending-closes.json")).await.unwrap()
        ).unwrap();
        assert!(pending.contains(&outgoing.session_id));
        app.shutdown().await;
        std::fs::remove_dir_all(root).unwrap();
    }

}
