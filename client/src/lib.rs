use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use starbridge_protocol::{
    AuthRequest, AuthResponse, ConnectionPolicy, DeviceInfo, DeviceRegistration, ErrorResponse,
    FrpRoute, HealthResponse, HeartbeatRequest, Role, SessionCreateRequest, SessionInfo,
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

#[cfg(feature = "desktop")]
pub mod desktop;

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
            ssh_user: current_user(),
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

/// 本机同时具备 SSH 登录密钥和接收连接的服务能力。
#[derive(Clone, Debug, Default, Serialize)]
pub struct SshStatus {
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
    pub display_name: String,
    pub devices: Vec<DeviceInfo>,
    pub session: Option<SessionView>,
    pub sessions: Vec<SessionView>,
    pub stage: String,
    pub last_error: Option<String>,
    pub runtime: RuntimeStatus,
    pub ssh: SshStatus,
    pub ssh_command: Option<String>,
    pub updated_at: u64,
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
    stage: String,
    last_error: Option<String>,
    runtime: RuntimeStatus,
    ssh: SshStatus,
    visitor_ports: HashMap<String, u16>,
    route_generations: HashMap<String, u64>,
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
    pub async fn load(_role: Role, root: PathBuf) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(&root).await?;
        let config = load_json(&root.join("config.json"), ClientConfig::default()).await?;
        let identity = load_json(&root.join("identity.json"), Identity::default()).await?;
        let persisted_auth: Option<AuthResponse> =
            load_json(&root.join("auth.json"), None::<AuthResponse>).await?;
        let auth = persisted_auth.filter(|value| value.expires_at > now_unix());
        let pending_closes = load_json(&root.join("pending-closes.json"), HashSet::new()).await?;
        let restoring = auth.is_some();
        let runtime = runtime_status(&root);
        let app = Self {
            inner: Arc::new(RwLock::new(Inner {
                role: Role::Peer,
                root,
                config,
                identity,
                auth,
                devices: Vec::new(),
                sessions: HashMap::new(),
                pending_closes,
                stage: if restoring { "restoring" } else { "ready" }.into(),
                last_error: None,
                runtime,
                ssh: SshStatus::default(),
                visitor_ports: HashMap::new(),
                route_generations: HashMap::new(),
            })),
            http: reqwest::Client::builder().no_proxy().timeout(std::time::Duration::from_secs(15)).build()?,
            frpc_processes: Arc::new(Mutex::new(HashMap::new())),
            session_operations: Arc::new(Mutex::new(())),
            sshd_process: Arc::new(Mutex::new(None)),
            shutting_down: Arc::new(AtomicBool::new(false)),
            background_tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        app.prepare_ssh().await;
        Ok(app)
    }

    async fn prepare_ssh(&self) {
        let (root, runtime) = {
            let inner = self.inner.read().await;
            (inner.root.clone(), inner.runtime.clone())
        };
        let dir = root.join("ssh");
        let key = ensure_key(
            &runtime_file(&runtime, "ssh-keygen"),
            &dir.join("id_ed25519"),
        ).await;
        let mut status = self.start_sshd(&runtime, &dir).await.unwrap_or_else(|error| SshStatus {
            message: format!("接收 SSH 连接暂不可用：{error}"),
            ..SshStatus::default()
        });
        match key {
            Ok(public_key) => {
                status.public_key = public_key;
                status.message = format!("SSH 登录密钥已就绪 · {}", status.message);
            }
            Err(error) => status.message = format!("SSH 登录密钥准备失败：{error} · {}", status.message),
        }
        self.inner.write().await.ssh = status;
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
        let mut child = async_command(&sshd)
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
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("sshd 启动后退出: {status}，日志见 {}", log_path.display());
        }
        *self.sshd_process.lock().await = Some(child);
        let privilege = detect_privilege();
        Ok(SshStatus {
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
            .route("/api/health", get(health))
            .route("/api/v1/state", get(state))
            .route("/api/v1/config", get(get_config).put(update_config))
            .route("/api/v1/login", post(login))
            .route("/api/v1/logout", post(logout))
            .route("/api/v1/connect", post(connect))
            .route("/api/v1/disconnect", delete(disconnect))
            .route("/api/v1/sessions/{id}", delete(disconnect_session))
            .route("/api/v1/copy/nodes", get(copy_nodes))
            .with_state(self.clone())
    }

    pub async fn heartbeat_loop(&self) {
        let app = self.clone();
        self.spawn_background(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                interval.tick().await;
                let attempted_auth = app.inner.read().await.auth.as_ref().map(|auth| auth.access_token.clone());
                if let Err(error) = app.heartbeat_once().await {
                    let _operation = app.session_operations.lock().await;
                    let mut inner = app.inner.write().await;
                    if inner.auth.as_ref().map(|auth| &auth.access_token) != attempted_auth.as_ref() {
                        continue;
                    }
                    if inner.auth.is_some() {
                        if error.downcast_ref::<reqwest::Error>().and_then(reqwest::Error::status)
                            == Some(StatusCode::UNAUTHORIZED) {
                            inner.auth = None;
                            let _ = tokio::fs::remove_file(inner.root.join("auth.json")).await;
                            drop(inner);
                            app.close_sessions(None).await;
                            app.sync_authorized_keys(&[]).await;
                            inner = app.inner.write().await;
                            inner.stage = "ready".into();
                        } else {
                            inner.stage = "heartbeat_failed".into();
                        }
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

    async fn heartbeat_once(&self) -> anyhow::Result<()> {
        let _operation = self.session_operations.lock().await;
        let (auth, url, identity, registration) = {
            let inner = self.inner.read().await;
            (
                inner.auth.clone().ok_or_else(|| anyhow::anyhow!("未登录"))?,
                inner.config.server_url.clone(),
                inner.identity.clone(),
                device_registration(&inner),
            )
        };
        // 恢复登录或重启后重新发布本机密钥和能力，旧身份也升级为统一节点。
        let devices = self.http
            .post(format!("{url}/api/v1/devices/register"))
            .bearer_auth(&auth.access_token)
            .json(&registration)
            .send().await?.error_for_status()?
            .json::<Vec<DeviceInfo>>().await?;
        self.sync_authorized_keys(&devices).await;
        self.inner.write().await.devices = devices;
        // 断网时的断开意图持久保存；先补发撤销，不能把它当作待恢复连接。
        self.retry_pending_closes().await;
        let assigned = self.http
            .get(format!("{url}/api/v1/sessions/assigned/{}", identity.node_id))
            .bearer_auth(&auth.access_token)
            .header("x-starbridge-device-token", &identity.device_token)
            .send().await?.error_for_status()?
            .json::<Vec<SessionInfo>>().await?;
        let active: HashSet<_> = assigned.iter().map(|session| session.session_id.clone()).collect();
        {
            let mut inner = self.inner.write().await;
            inner.pending_closes.retain(|id| active.contains(id));
            save_json(&inner.root.join("pending-closes.json"), &inner.pending_closes).await?;
        }
        let stale: Vec<_> = self.inner.read().await.sessions.keys()
            .filter(|id| !active.contains(*id)).cloned().collect();
        for id in stale {
            self.stop_session(&id).await;
        }
        for session in assigned {
            if self.inner.read().await.pending_closes.contains(&session.session_id) {
                continue;
            }
            let outgoing = session.initiator_node_id == identity.node_id;
            let old = self.inner.read().await.sessions.get(&session.session_id).cloned();
            let running = {
                let mut processes = self.frpc_processes.lock().await;
                match processes.get_mut(&session.session_id) {
                    Some(child) => child.try_wait()?.is_none(),
                    None => false,
                }
            };
            let should_start = !running || old.as_ref().is_none_or(|old| {
                session.path == SessionPath::Relay && old.path != SessionPath::Relay
            });
            self.inner.write().await.sessions.insert(session.session_id.clone(), session.clone());
            if should_start {
                match self.start_frpc_mode(&session, outgoing, session.path == SessionPath::Relay).await {
                    Ok(()) => {
                        if outgoing {
                            self.spawn_direct_fallback_monitor(session.clone()).await;
                        }
                        let path = if session.policy == ConnectionPolicy::RelayOnly {
                            SessionPath::Relay
                        } else {
                            session.path
                        };
                        if let Ok(updated) = update_remote_session(self, &session, SessionStatus::Connected, path, session.failure_reason.clone()).await {
                            self.inner.write().await.sessions.insert(updated.session_id.clone(), updated);
                        }
                    }
                    Err(error) => self.fail_session(&session, error.to_string()).await,
                }
            }
        }
        let mut inner = self.inner.write().await;
        if inner.sessions.is_empty() || inner.stage == "heartbeat_failed" {
            inner.stage = "online".into();
            if inner.pending_closes.is_empty() { inner.last_error = None; }
        }
        Ok(())
    }

    async fn fail_session(&self, session: &SessionInfo, reason: String) {
        self.stop_session(&session.session_id).await;
        let _ = update_remote_session(self, session, SessionStatus::Failed, SessionPath::Unknown, Some(reason.clone())).await;
        let mut inner = self.inner.write().await;
        inner.stage = "frpc_failed".into();
        inner.last_error = Some(format!("连接启动失败：{reason}。{}", security_hint()));
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
        if self.shutting_down.load(Ordering::Acquire) {
            anyhow::bail!("客户端正在退出");
        }
        let route = session
            .route
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("中心没有返回 FRP 路由参数"))?;
        let (root, frpc_path, ssh_port, existing_port, target) = {
            let inner = self.inner.read().await;
            (
                inner.root.clone(),
                inner.runtime.frpc_path.clone(),
                inner.ssh.port,
                inner.visitor_ports.get(&session.session_id).copied(),
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
            match existing_port {
                Some(port) => port,
                None => pick_port(22000, 22999)?,
            }
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
        // 在首次 await 前登记，后台任务被取消时 shutdown 仍能等待进程退出。
        current.insert(session.session_id.clone(), child);
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        if let Some(status) = current.get_mut(&session.session_id).expect("frpc process registered").try_wait()? {
            current.remove(&session.session_id);
            anyhow::bail!("frpc 启动后退出: {status}，日志见 {}", log_path.display());
        }
        drop(current);
        let mut inner = self.inner.write().await;
        *inner.route_generations.entry(session.session_id.clone()).or_default() += 1;
        if controller {
            inner.visitor_ports.insert(session.session_id.clone(), port);
        }
        Ok(())
    }

    /// 跟踪 visitor 日志判定实际路径。XTCP 要等第一条 SSH 连接才开始打洞，
    /// 所以一直跟踪到会话结束，而不是只看启动后的固定时长。
    async fn spawn_direct_fallback_monitor(&self, session: SessionInfo) {
        if session.policy == ConnectionPolicy::RelayOnly || session.path == SessionPath::Relay {
            return;
        }
        let generation = self.inner.read().await.route_generations.get(&session.session_id).copied();
        let app = self.clone();
        self.spawn_background(async move {
            let root = { app.inner.read().await.root.clone() };
            let log_path = root
                .join("routes")
                .join(format!("{}.log", session.session_id));
            let mut reported_failure = false;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let active = app
                    .inner
                    .read()
                    .await
                    .route_generations
                    .get(&session.session_id).copied() == generation;
                if !active {
                    break;
                }
                let log = match tokio::fs::read_to_string(&log_path).await {
                    Ok(log) => log,
                    Err(_) => continue,
                };
                match frpc_path_from_log(&log) {
                    Some(SessionPath::Direct) => {
                        let _operation = app.session_operations.lock().await;
                        if app.inner.read().await.route_generations.get(&session.session_id).copied() != generation { break; }
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
                            inner.sessions.insert(updated.session_id.clone(), updated);
                            inner.stage = "direct".into();
                            inner.last_error = None;
                        }
                        break;
                    }
                    Some(SessionPath::Relay)
                        if session.policy == ConnectionPolicy::DirectPreferred =>
                    {
                        let _operation = app.session_operations.lock().await;
                        if app.inner.read().await.route_generations.get(&session.session_id).copied() != generation { break; }
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
                                inner.sessions.insert(updated.session_id.clone(), updated);
                                inner.stage = "relay_fallback".into();
                                inner.last_error = Some("XTCP 直连失败，已回退到 FRPS 中转".into());
                            }
                        }
                        break;
                    }
                    // 只允许直连时不回退；记录失败原因，visitor 会在下一条连接时重新打洞。
                    Some(SessionPath::Relay) if !reported_failure => {
                        let _operation = app.session_operations.lock().await;
                        if app.inner.read().await.route_generations.get(&session.session_id).copied() != generation { break; }
                        reported_failure = true;
                        let reason = "XTCP 直连失败，本次连接只允许直连，不会回退到中转";
                        let _ = update_remote_session(
                            &app,
                            &session,
                            SessionStatus::Connected,
                            SessionPath::Unknown,
                            Some(reason.into()),
                        )
                        .await;
                        let mut inner = app.inner.write().await;
                        inner.stage = "direct_failed".into();
                        inner.last_error = Some(reason.into());
                    }
                    _ => {}
                }
            }
        });
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
    if !inner.ssh.public_key.is_empty() && inner.runtime.ssh_available {
        capabilities.push("ssh_client".into());
    }
    if inner.ssh.port.is_some() {
        capabilities.extend(["ssh_server", "ssh", "sftp", "scp"].map(String::from));
    }
    DeviceRegistration {
        node_id: inner.identity.node_id.clone(),
        device_token: inner.identity.device_token.clone(),
        display_name: inner.config.display_name.clone(),
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
    Ok(Json(inner.config.clone()))
}

async fn login(
    State(app): State<ClientApp>,
    Json(form): Json<LoginForm>,
) -> Result<Json<ClientSnapshot>, ApiError> {
    if form.username.trim().is_empty() || form.password.len() < 4 {
        return Err(ApiError::bad_request("请输入账户名称和至少 4 位密码"));
    }
    let _operation = app.session_operations.lock().await;
    app.close_sessions(None).await;
    let (url, identity, registration) = {
        let inner = app.inner.read().await;
        (inner.config.server_url.clone(), inner.identity.clone(), device_registration(&inner))
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
    app.sync_authorized_keys(&devices).await;
    let mut inner = app.inner.write().await;
    inner.auth = Some(auth.clone());
    inner.devices = devices;
    inner.stage = "online".into();
    inner.last_error = None;
    save_json(&inner.root.join("identity.json"), &identity)
        .await
        .map_err(ApiError::internal)?;
    save_json(&inner.root.join("auth.json"), &auth)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(snapshot(&inner)))
}

async fn logout(State(app): State<ClientApp>) -> Json<ClientSnapshot> {
    let _operation = app.session_operations.lock().await;
    app.close_sessions(None).await;
    app.mark_offline().await;
    app.sync_authorized_keys(&[]).await;
    let mut inner = app.inner.write().await;
    inner.auth = None;
    inner.devices.clear();
    inner.stage = "ready".into();
    let _ = tokio::fs::remove_file(inner.root.join("auth.json")).await;
    Json(snapshot(&inner))
}

async fn connect(
    State(app): State<ClientApp>,
    Json(form): Json<ConnectForm>,
) -> Result<Json<ClientSnapshot>, ApiError> {
    let _operation = app.session_operations.lock().await;
    let (auth, url, identity) = {
        let inner = app.inner.read().await;
        if inner.ssh.public_key.is_empty() {
            return Err(ApiError::bad_request("本机 SSH 登录密钥尚未就绪"));
        }
        if form.target_node_id == inner.identity.node_id {
            return Err(ApiError::bad_request("请选择另一台设备"));
        }
        (
            inner.auth.clone().ok_or_else(|| ApiError::unauthorized("请先登录"))?,
            inner.config.server_url.clone(),
            inner.identity.clone(),
        )
    };
    // 同一目标重复连接只替换同向会话；反向和其他设备的连接保持运行。
    let replaced: Vec<_> = app.inner.read().await.sessions.values()
        .filter(|session| session.initiator_node_id == identity.node_id && session.target_node_id == form.target_node_id)
        .map(|session| session.session_id.clone()).collect();
    for id in replaced {
        app.close_sessions(Some(&id)).await;
    }
    let info = app.http
        .post(format!("{url}/api/v1/sessions"))
        .bearer_auth(&auth.access_token)
        .json(&SessionCreateRequest {
            initiator_node_id: identity.node_id,
            device_token: identity.device_token,
            target_node_id: form.target_node_id,
            policy: form.policy,
        })
        .send().await.map_err(ApiError::network)?
        .error_for_status().map_err(ApiError::from_response)?
        .json::<SessionInfo>().await.map_err(ApiError::network)?;
    app.inner.write().await.sessions.insert(info.session_id.clone(), info.clone());
    let runtime = app.inner.read().await.runtime.clone();
    let result = match probe_frpc(&runtime.frpc_path) {
        Ok(_) => app.start_frpc(&info, true).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => {
            if let Ok(updated) = update_remote_session(&app, &info, SessionStatus::Connected, initial_session_path(info.policy), None).await {
                app.inner.write().await.sessions.insert(updated.session_id.clone(), updated);
            }
            let mut inner = app.inner.write().await;
            inner.stage = "frpc_running".into();
            inner.last_error = None;
            drop(inner);
            app.spawn_direct_fallback_monitor(info).await;
        }
        Err(error) => app.fail_session(&info, error.to_string()).await,
    }
    let inner = app.inner.read().await;
    Ok(Json(snapshot(&inner)))
}

async fn disconnect(State(app): State<ClientApp>) -> Result<Json<ClientSnapshot>, ApiError> {
    let _operation = app.session_operations.lock().await;
    app.close_sessions(None).await;
    let inner = app.inner.read().await;
    Ok(Json(snapshot(&inner)))
}

async fn disconnect_session(
    State(app): State<ClientApp>,
    Path(id): Path<String>,
) -> Result<Json<ClientSnapshot>, ApiError> {
    let _operation = app.session_operations.lock().await;
    app.close_sessions(Some(&id)).await;
    let inner = app.inner.read().await;
    Ok(Json(snapshot(&inner)))
}

async fn copy_nodes(State(app): State<ClientApp>) -> Json<serde_json::Value> {
    let inner = app.inner.read().await;
    let text = inner
        .devices
        .iter()
        .filter(|d| d.node_id != inner.identity.node_id && d.online && d.capabilities.iter().any(|capability| capability == "ssh_server"))
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
        display_name: inner.config.display_name.clone(),
        devices: inner.devices.clone(),
        ssh_command: primary.as_ref().and_then(|session| session.ssh_command.clone()),
        session: primary,
        sessions,
        stage: inner.stage.clone(),
        last_error: inner.last_error.clone(),
        runtime: inner.runtime.clone(),
        ssh: inner.ssh.clone(),
        updated_at: now_unix(),
    }
}

fn session_ssh_command(inner: &Inner, session: &SessionInfo) -> Option<String> {
    if session.initiator_node_id != inner.identity.node_id { return None; }
    inner.visitor_ports.get(&session.session_id)?;
    let target = inner.devices.iter().find(|device| device.node_id == session.target_node_id)?;
    let config = std::path::absolute(inner.root.join("ssh").join("sessions").join(&session.session_id).join("config")).ok()?;
    Some(format!("ssh -F \"{}\" {}", config.display(), target.ssh_alias))
}

fn runtime_status(root: &FsPath) -> RuntimeStatus {
    let executable = std::env::current_exe().ok();
    let runtime = runtime_directory(root, executable.as_deref().and_then(FsPath::parent));
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

fn runtime_directory(root: &FsPath, executable_dir: Option<&FsPath>) -> PathBuf {
    let local_runtime = root.join("runtime");
    std::iter::once(local_runtime.clone())
        .chain(root.parent().map(|parent| parent.join("runtime")))
        .chain(executable_dir.map(|directory| directory.join("runtime")))
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

fn probe_frpc(path: &str) -> anyhow::Result<String> {
    let output = quiet_command(path)
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
    // 源码构建的 sshd 把 sshd-session/sshd-auth 的路径编译成安装前缀，需要改指运行时目录。
    if !cfg!(windows) {
        let runtime = sftp_server.parent().unwrap_or(FsPath::new(""));
        config.push_str(&format!(
            "SshdSessionPath {}\nSshdAuthPath {}\n",
            ssh_path(&runtime.join("sshd-session")),
            ssh_path(&runtime.join("sshd-auth")),
        ));
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

fn initial_session_path(policy: ConnectionPolicy) -> SessionPath {
    match policy {
        ConnectionPolicy::RelayOnly => SessionPath::Relay,
        ConnectionPolicy::DirectOnly | ConnectionPolicy::DirectPreferred => SessionPath::Unknown,
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

/// frpc/sshd 被拦截时给用户的人工排查提示；StarBridge 不自动修改系统安全设置。
fn security_hint() -> &'static str {
    if cfg!(windows) {
        "请检查 Windows Defender、杀毒软件或企业安全策略；StarBridge 不会自动修改白名单。"
    } else if cfg!(target_os = "macos") {
        "请检查“系统设置 → 隐私与安全性”或企业安全软件是否阻止了该程序；StarBridge 不会自动修改安全设置。"
    } else {
        "请检查安全软件或系统策略是否阻止了该程序；StarBridge 不会自动修改安全设置。"
    }
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
            .start_frpc(&session(ConnectionPolicy::RelayOnly), true)
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
            inner.auth = Some(AuthResponse { access_token: "test".into(), username: "alice".into(), expires_at: u64::MAX });
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

    #[test]
    fn relay_only_is_known_before_connecting() {
        assert_eq!(
            initial_session_path(ConnectionPolicy::RelayOnly),
            SessionPath::Relay
        );
        assert_eq!(
            initial_session_path(ConnectionPolicy::DirectOnly),
            SessionPath::Unknown
        );
    }
}
