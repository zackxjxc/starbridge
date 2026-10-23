use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use starbridge_protocol::{
    AuthRequest, AuthResponse, ConnectionPolicy, DeviceInfo, DeviceRegistration, ErrorResponse,
    FrpRoute, HealthResponse, HeartbeatRequest, Role, SessionCreateRequest, SessionInfo,
    SessionPath, SessionStatus, SessionStatusRequest, new_id, now_unix,
};
use std::{
    collections::HashMap,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;

const ADMIN_HTML: &str = include_str!("../../admin-ui/index.html");

#[derive(Clone)]
pub struct AppState {
    inner: Arc<RwLock<Inner>>,
    data_path: Arc<PathBuf>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Account {
    password: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct StoredDevice {
    account: String,
    device_token: String,
    registration: DeviceRegistration,
    last_heartbeat: u64,
    status: String,
}

#[derive(Default)]
struct Inner {
    accounts: HashMap<String, Account>,
    tokens: HashMap<String, (String, u64)>,
    devices: HashMap<String, StoredDevice>,
    sessions: HashMap<String, SessionInfo>,
    frp_server_addr: String,
    frp_server_port: u16,
    frp_auth_token: String,
}

#[derive(Deserialize, Serialize)]
struct PersistedState {
    accounts: HashMap<String, Account>,
    devices: HashMap<String, StoredDevice>,
    sessions: HashMap<String, SessionInfo>,
}

impl AppState {
    pub fn new() -> Self {
        let address = std::env::var("STARBRIDGE_FRP_ADDR").unwrap_or_else(|_| "127.0.0.1".into());
        let port = std::env::var("STARBRIDGE_FRP_PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(17851);
        let token =
            std::env::var("STARBRIDGE_FRP_TOKEN").unwrap_or_else(|_| "starbridge-dev-token".into());
        Self::with_frp(address, port, token)
    }

    pub fn with_frp(address: String, port: u16, token: String) -> Self {
        Self::with_frp_and_data(address, port, token, PathBuf::from("state/server"))
    }

    pub fn with_frp_and_data(
        address: String,
        port: u16,
        token: String,
        data_path: PathBuf,
    ) -> Self {
        let persisted = load_persisted(&data_path);
        let sessions = persisted
            .as_ref()
            .map(|state| {
                state
                    .sessions
                    .clone()
                    .into_iter()
                    .map(|(id, mut session)| {
                        session.status = SessionStatus::Closed;
                        session.failure_reason = Some("中心服务重启，会话已结束".into());
                        (id, session)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            inner: Arc::new(RwLock::new(Inner {
                accounts: persisted
                    .as_ref()
                    .map(|state| state.accounts.clone())
                    .unwrap_or_default(),
                devices: persisted
                    .as_ref()
                    .map(|state| state.devices.clone())
                    .unwrap_or_default(),
                sessions,
                frp_server_addr: address,
                frp_server_port: port,
                frp_auth_token: token,
                ..Inner::default()
            })),
            data_path: Arc::new(data_path),
        }
    }

    async fn persist(&self) {
        let inner = self.inner.read().await;
        if let Err(error) = persist_inner(&self.data_path, &inner) {
            tracing::error!(%error, "保存中心服务状态失败");
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

fn load_persisted(data_path: &FsPath) -> Option<PersistedState> {
    let path = data_path.join("server-state.json");
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::warn!(%error, "中心服务状态文件无法解析，将从空状态启动");
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(%error, "中心服务状态文件无法读取，将从空状态启动");
            None
        }
    }
}

fn persist_inner(data_path: &FsPath, inner: &Inner) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_path)?;
    let bytes = serde_json::to_vec_pretty(&PersistedState {
        accounts: inner.accounts.clone(),
        devices: inner.devices.clone(),
        sessions: inner.sessions.clone(),
    })?;
    let path = data_path.join("server-state.json");
    let pending = data_path.join("server-state.pending");
    std::fs::write(&pending, bytes)?;
    std::fs::rename(pending, path)?;
    Ok(())
}

fn password_digest(password: &str) -> String {
    format!("{:x}", Sha256::digest(password.as_bytes()))
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(admin))
        .route("/admin", get(admin))
        .route("/api/health", get(health))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/devices/register", post(register_device))
        .route("/api/v1/devices/heartbeat", post(heartbeat))
        .route("/api/v1/devices", get(devices))
        .route("/api/v1/admin/summary", get(admin_summary))
        .route("/api/v1/sessions", post(create_session))
        .route(
            "/api/v1/sessions/assigned/{node_id}",
            get(assigned_sessions),
        )
        .route(
            "/api/v1/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/api/v1/sessions/{id}/status", post(update_session))
        .with_state(state)
}

async fn admin() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        service: "starbridge-server",
        version: env!("CARGO_PKG_VERSION"),
        now: now_unix(),
    })
}

async fn login(
    State(state): State<AppState>,
    Json(request): Json<AuthRequest>,
) -> Result<Json<AuthResponse>, ApiError> {
    let username = request.username.trim().to_string();
    if username.is_empty() || username.len() > 128 || request.password.len() < 4 {
        return Err(ApiError::bad_request(
            "用户名不能为空，密码至少需要 4 个字符",
        ));
    }
    let password_hash = password_digest(&request.password);
    let mut inner = state.inner.write().await;
    if let Some(account) = inner.accounts.get(&username) {
        if account.password != password_hash {
            return Err(ApiError::unauthorized("账户密码不正确"));
        }
    } else {
        inner.accounts.insert(
            username.clone(),
            Account {
                password: password_hash,
            },
        );
    }
    let token = starbridge_protocol::new_token();
    let expires_at = now_unix() + 86_400;
    inner
        .tokens
        .insert(token.clone(), (username.clone(), expires_at));
    let response = Json(AuthResponse {
        access_token: token,
        username,
        expires_at,
    });
    drop(inner);
    state.persist().await;
    Ok(response)
}

async fn register_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(registration): Json<DeviceRegistration>,
) -> Result<Json<Vec<DeviceInfo>>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    validate_registration(&registration)?;
    let mut inner = state.inner.write().await;
    if registration.role == Role::Controller && registration.direct_only && registration.allow_relay
    {
        return Err(ApiError::bad_request("只允许直连的节点不能同时允许中继"));
    }
    if let Some(existing) = inner.devices.get(&registration.node_id) {
        if existing.account != account || existing.device_token != registration.device_token {
            return Err(ApiError::conflict("设备身份凭据与已有登记不一致"));
        }
    }
    let account_name = account.clone();
    inner.devices.insert(
        registration.node_id.clone(),
        StoredDevice {
            account,
            device_token: registration.device_token.clone(),
            registration,
            last_heartbeat: now_unix(),
            status: "online".to_string(),
        },
    );
    let response = Json(list_devices(&inner, Some(&account_name)));
    drop(inner);
    state.persist().await;
    Ok(response)
}

async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<HeartbeatRequest>,
) -> Result<Json<Vec<DeviceInfo>>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let mut inner = state.inner.write().await;
    let device = inner
        .devices
        .get_mut(&request.node_id)
        .ok_or_else(|| ApiError::not_found("设备尚未登记"))?;
    if device.account != account {
        return Err(ApiError::forbidden("不能更新其他账户的设备"));
    }
    device.last_heartbeat = now_unix();
    device.status = request.status;
    Ok(Json(list_devices(&inner, Some(&account))))
}

async fn devices(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<DeviceInfo>>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let inner = state.inner.read().await;
    Ok(Json(list_devices(&inner, Some(&account))))
}

#[derive(Serialize)]
struct AdminSummary {
    account: String,
    devices: Vec<DeviceInfo>,
    sessions: Vec<SessionInfo>,
}

async fn admin_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AdminSummary>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let inner = state.inner.read().await;
    let device_ids: std::collections::HashSet<_> = inner
        .devices
        .values()
        .filter(|device| device.account == account)
        .map(|device| device.registration.node_id.clone())
        .collect();
    let sessions = inner
        .sessions
        .values()
        .filter(|session| {
            device_ids.contains(&session.controller_node_id)
                || device_ids.contains(&session.target_node_id)
        })
        .cloned()
        .collect();
    Ok(Json(AdminSummary {
        account: account.clone(),
        devices: list_devices(&inner, Some(&account)),
        sessions,
    }))
}

async fn create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SessionCreateRequest>,
) -> Result<Json<SessionInfo>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let mut inner = state.inner.write().await;
    let controller = inner
        .devices
        .values()
        .find(|device| {
            device.account == account
                && device.registration.role == Role::Controller
                && is_online(device)
        })
        .map(|device| device.registration.clone())
        .ok_or_else(|| ApiError::conflict("当前账户没有在线控制端节点"))?;
    let target = inner
        .devices
        .get(&request.target_node_id)
        .ok_or_else(|| ApiError::not_found("执行端节点不存在"))?;
    if target.account != account || target.registration.role != Role::Executor {
        return Err(ApiError::forbidden("不能连接这个执行端节点"));
    }
    if !is_online(target) {
        return Err(ApiError::conflict("执行端节点当前离线"));
    }
    let target_registration = target.registration.clone();
    let policy = effective_policy(request.policy, &controller, &target_registration)?;
    let created_at = now_unix();
    let session_id = new_id();
    let proxy_name = format!(
        "sb-{}-{}",
        &target_registration.node_id[..8.min(target_registration.node_id.len())],
        &session_id[..8]
    );
    let info = SessionInfo {
        session_id,
        controller_node_id: controller.node_id.clone(),
        target_node_id: target_registration.node_id,
        policy,
        path: SessionPath::Unknown,
        status: SessionStatus::Negotiating,
        created_at,
        expires_at: created_at + 600,
        failure_reason: None,
        route: Some(FrpRoute {
            server_addr: inner.frp_server_addr.clone(),
            server_port: inner.frp_server_port,
            auth_token: inner.frp_auth_token.clone(),
            proxy_name,
            secret_key: starbridge_protocol::new_token(),
            remote_port: 20000 + (inner.sessions.len() as u16 % 20000),
        }),
    };
    inner.sessions.insert(info.session_id.clone(), info.clone());
    drop(inner);
    state.persist().await;
    Ok(Json(info))
}

async fn assigned_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(node_id): Path<String>,
) -> Result<Json<Vec<SessionInfo>>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let inner = state.inner.read().await;
    let device = inner
        .devices
        .get(&node_id)
        .ok_or_else(|| ApiError::not_found("设备不存在"))?;
    if device.account != account {
        return Err(ApiError::forbidden("不能读取其他账户的会话"));
    }
    Ok(Json(
        inner
            .sessions
            .values()
            .filter(|session| {
                session.target_node_id == node_id
                    && !matches!(
                        session.status,
                        SessionStatus::Closed | SessionStatus::Failed
                    )
            })
            .cloned()
            .collect(),
    ))
}

async fn get_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<SessionInfo>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let inner = state.inner.read().await;
    let session = inner
        .sessions
        .get(&id)
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    ensure_session_account(&inner, session, &account)?;
    Ok(Json(session.clone()))
}

async fn update_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<SessionStatusRequest>,
) -> Result<Json<SessionInfo>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let mut inner = state.inner.write().await;
    let existing = inner
        .sessions
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    ensure_session_account(&inner, &existing, &account)?;
    let session = inner.sessions.get_mut(&id).expect("session exists");
    session.status = request.status;
    session.path = request.path;
    session.failure_reason = request.failure_reason;
    let response = Json(session.clone());
    drop(inner);
    state.persist().await;
    Ok(response)
}

async fn delete_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<SessionInfo>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let mut inner = state.inner.write().await;
    let existing = inner
        .sessions
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    ensure_session_account(&inner, &existing, &account)?;
    let session = inner.sessions.get_mut(&id).expect("session exists");
    session.status = SessionStatus::Closed;
    let response = Json(session.clone());
    drop(inner);
    state.persist().await;
    Ok(response)
}

fn validate_registration(registration: &DeviceRegistration) -> Result<(), ApiError> {
    if registration.node_id.trim().is_empty()
        || registration.node_id.len() > 128
        || registration.display_name.trim().is_empty()
        || registration.display_name.len() > 80
        || registration.ssh_alias.trim().is_empty()
    {
        return Err(ApiError::bad_request("设备身份或显示名称格式错误"));
    }
    if registration.capabilities.is_empty() {
        return Err(ApiError::bad_request("设备至少需要声明一种能力"));
    }
    Ok(())
}

fn effective_policy(
    requested: ConnectionPolicy,
    controller: &DeviceRegistration,
    target: &DeviceRegistration,
) -> Result<ConnectionPolicy, ApiError> {
    if requested == ConnectionPolicy::RelayOnly
        && (controller.direct_only
            || target.direct_only
            || !controller.allow_relay
            || !target.allow_relay)
    {
        return Err(ApiError::conflict("节点策略禁止使用中继"));
    }
    if requested == ConnectionPolicy::DirectPreferred
        && (controller.direct_only || target.direct_only)
    {
        return Ok(ConnectionPolicy::DirectOnly);
    }
    if requested != ConnectionPolicy::DirectOnly && (!controller.allow_relay || !target.allow_relay)
    {
        return Ok(ConnectionPolicy::DirectOnly);
    }
    Ok(requested)
}

fn list_devices(inner: &Inner, account: Option<&str>) -> Vec<DeviceInfo> {
    inner
        .devices
        .values()
        .filter(|device| account.is_none_or(|value| device.account == value))
        .map(|device| DeviceInfo {
            node_id: device.registration.node_id.clone(),
            account: device.account.clone(),
            display_name: device.registration.display_name.clone(),
            role: device.registration.role,
            ssh_alias: device.registration.ssh_alias.clone(),
            ssh_user: device.registration.ssh_user.clone(),
            capabilities: device.registration.capabilities.clone(),
            allow_relay: device.registration.allow_relay,
            direct_only: device.registration.direct_only,
            version: device.registration.version.clone(),
            os: device.registration.os.clone(),
            arch: device.registration.arch.clone(),
            hostname: device.registration.hostname.clone(),
            online: is_online(device),
            last_heartbeat: device.last_heartbeat,
        })
        .collect()
}

fn is_online(device: &StoredDevice) -> bool {
    now_unix().saturating_sub(device.last_heartbeat) <= 30
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::unauthorized("需要 Bearer 访问令牌"))?;
    let inner = state.inner.read().await;
    let (username, expires_at) = inner
        .tokens
        .get(value)
        .cloned()
        .ok_or_else(|| ApiError::unauthorized("访问令牌无效或已过期"))?;
    inner
        .accounts
        .get(&username)
        .ok_or_else(|| ApiError::unauthorized("账户不存在"))?;
    if expires_at <= now_unix() {
        return Err(ApiError::unauthorized("访问令牌已过期"));
    }
    Ok(username)
}

fn ensure_session_account(
    inner: &Inner,
    session: &SessionInfo,
    account: &str,
) -> Result<(), ApiError> {
    let controller = inner.devices.get(&session.controller_node_id);
    let target = inner.devices.get(&session.target_node_id);
    if controller.is_some_and(|d| d.account == account)
        || target.is_some_and(|d| d.account == account)
    {
        Ok(())
    } else {
        Err(ApiError::forbidden("不能访问其他账户的会话"))
    }
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
    fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
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

    fn device(direct_only: bool, allow_relay: bool) -> DeviceRegistration {
        DeviceRegistration {
            node_id: "node".into(),
            device_token: "token".into(),
            display_name: "node".into(),
            role: Role::Controller,
            ssh_alias: "node".into(),
            ssh_user: "user".into(),
            capabilities: vec!["ssh".into()],
            allow_relay,
            direct_only,
            version: "test".into(),
            os: "windows".into(),
            arch: "x86_64".into(),
            hostname: "node".into(),
        }
    }

    #[test]
    fn direct_only_device_restricts_preferred_policy() {
        let controller = device(false, true);
        let target = device(true, false);
        assert_eq!(
            effective_policy(ConnectionPolicy::DirectPreferred, &controller, &target).unwrap(),
            ConnectionPolicy::DirectOnly
        );
    }

    #[test]
    fn relay_only_is_rejected_when_any_side_disallows_relay() {
        let controller = device(false, true);
        let target = device(true, false);
        assert!(effective_policy(ConnectionPolicy::RelayOnly, &controller, &target).is_err());
    }

    #[test]
    fn device_listing_is_scoped_to_account() {
        let mut inner = Inner::default();
        for (account, node_id) in [("alice", "a"), ("bob", "b")] {
            let registration = DeviceRegistration {
                node_id: node_id.into(),
                device_token: format!("{node_id}-token"),
                display_name: node_id.into(),
                role: Role::Executor,
                ssh_alias: node_id.into(),
                ssh_user: "user".into(),
                capabilities: vec!["ssh".into()],
                allow_relay: true,
                direct_only: false,
                version: "test".into(),
                os: "windows".into(),
                arch: "x86_64".into(),
                hostname: node_id.into(),
            };
            inner.devices.insert(
                node_id.into(),
                StoredDevice {
                    account: account.into(),
                    device_token: format!("{node_id}-token"),
                    registration,
                    last_heartbeat: now_unix(),
                    status: "online".into(),
                },
            );
        }
        let devices = list_devices(&inner, Some("alice"));
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].account, "alice");
    }

    #[test]
    fn persisted_server_state_round_trips() {
        let directory = std::env::temp_dir().join(format!("starbridge-server-test-{}", new_id()));
        let mut inner = Inner::default();
        inner.accounts.insert(
            "alice".into(),
            Account {
                password: password_digest("test1234"),
            },
        );
        persist_inner(&directory, &inner).unwrap();
        let loaded = load_persisted(&directory).expect("state should load");
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(
            loaded.accounts["alice"].password,
            password_digest("test1234")
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
