use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use starbridge_protocol::{
    AuthRequest, AuthResponse, AuthRefreshRequest, AuthLogoutRequest, ConnectionPolicy,
    DeviceInfo, DeviceRegistration, DeviceRegistrationResponse, DeviceRebindRequest, DeviceMetadataUpdate, ErrorResponse,
    FrpRoute, HealthResponse, HeartbeatRequest, SessionCreateRequest, SessionInfo,
    SessionHistoryEntry, SessionHistoryPage, SessionHistoryQuery,
    SessionPath, SessionStatus, SessionStatusRequest, new_id, now_unix,
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, RwLock};

mod diagnostics;
mod route_ready;
pub use diagnostics::{DiagnosticIntegration, diagnostic_plugin_app};

const ADMIN_HTML: &str = include_str!("../../admin-ui/index.html");

#[derive(Clone)]
pub struct AppState {
    inner: Arc<RwLock<Inner>>,
    data_path: Arc<PathBuf>,
    persist_lock: Arc<Mutex<()>>,
    diagnostics: Arc<RwLock<Option<DiagnosticIntegration>>>,
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
    #[serde(default)]
    metadata_edited: bool,
}

#[derive(Clone, Deserialize, Serialize)]
struct DeletedDevice {
    account: String,
    revoked_token_hashes: HashSet<String>,
    #[serde(default)]
    machine_key: Option<String>,
    #[serde(default)]
    display_name: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct RefreshGrant {
    username: String,
    expires_at: u64,
    access_token_hash: String,
}

#[derive(Default)]
struct Inner {
    accounts: HashMap<String, Account>,
    tokens: HashMap<String, (String, u64)>,
    refresh_grants: HashMap<String, RefreshGrant>,
    devices: HashMap<String, StoredDevice>,
    deleted_devices: HashMap<String, DeletedDevice>,
    sessions: HashMap<String, SessionInfo>,
    diagnostics: HashMap<String, diagnostics::DiagnosticRecord>,
    frp_server_addr: String,
    frp_server_port: u16,
    frp_auth_token: String,
}

#[derive(Deserialize, Serialize)]
struct PersistedState {
    accounts: HashMap<String, Account>,
    devices: HashMap<String, StoredDevice>,
    #[serde(default)]
    deleted_devices: HashMap<String, DeletedDevice>,
    sessions: HashMap<String, SessionInfo>,
    #[serde(default)]
    refresh_grants: HashMap<String, RefreshGrant>,
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
                        if is_active(&session) {
                            session.status = SessionStatus::Closed;
                            session.failure_reason = Some("中心服务重启，会话已结束".into());
                        }
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
                deleted_devices: persisted.as_ref().map(|state| state.deleted_devices.clone()).unwrap_or_default(),
                sessions,
                refresh_grants: persisted.as_ref().map(|state| state.refresh_grants.clone()).unwrap_or_default(),
                frp_server_addr: address,
                frp_server_port: port,
                frp_auth_token: token,
                ..Inner::default()
            })),
            data_path: Arc::new(data_path),
            persist_lock: Arc::new(Mutex::new(())),
            diagnostics: Arc::new(RwLock::new(None)),
        }
    }

    async fn persist(&self) {
        let _writing = self.persist_lock.lock().await;
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
                tracing::warn!(line = error.line(), column = error.column(), "中心服务状态文件无法解析，将从空状态启动");
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
        deleted_devices: inner.deleted_devices.clone(),
        sessions: inner.sessions.clone(),
        refresh_grants: inner.refresh_grants.clone(),
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
        .route("/api/v1/auth/register", post(register_account))
        .route("/api/v1/auth/refresh", post(refresh_auth))
        .route("/api/v1/auth/logout", post(logout_auth))
        .route("/api/v1/devices/register", post(register_device))
        .route("/api/v1/devices/identify", post(identify_device))
        .route("/api/v1/devices/rebind", post(rebind_device))
        .route("/api/v1/devices/heartbeat", post(heartbeat))
        .route("/api/v1/devices", get(devices))
        .route("/api/v1/devices/{id}", axum::routing::put(update_device_metadata).delete(delete_device))
        .route("/api/v1/admin/summary", get(admin_summary))
        .route("/api/v1/sessions", post(create_session))
        .route("/api/v1/sessions/history/{node_id}", get(session_history))
        .route(
            "/api/v1/sessions/assigned/{node_id}",
            get(assigned_sessions),
        )
        .route(
            "/api/v1/sessions/{id}",
            get(route_ready::get_with_readiness).delete(delete_session),
        )
        .route("/api/v1/sessions/{id}/status", post(update_session))
        .route("/api/v1/diagnostics", post(diagnostics::create))
        .route("/api/v1/diagnostics/current/{node_id}", get(diagnostics::current).delete(diagnostics::close_current))
        .route("/api/v1/diagnostics/{id}", get(diagnostics::get).delete(diagnostics::close))
        .route("/api/v1/diagnostics/{id}/status", post(diagnostics::update))
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
    let mut inner = state.inner.write().await;
    if !inner.accounts.get(&username).is_some_and(|account| account.password == password_digest(&request.password)) {
        return Err(ApiError::unauthorized("账户不存在或密码不正确，请检查后登录或注册"));
    }
    let response = issue_auth(&mut inner, username);
    persist_inner(&state.data_path, &inner).map_err(ApiError::internal)?;
    Ok(Json(response))
}

async fn register_account(
    State(state): State<AppState>,
    Json(request): Json<AuthRequest>,
) -> Result<Json<AuthResponse>, ApiError> {
    let username = request.username.trim().to_string();
    if username.is_empty() || username.len() > 128 || request.password.len() < 4 {
        return Err(ApiError::bad_request("用户名不能为空，密码至少需要 4 个字符"));
    }
    let mut inner = state.inner.write().await;
    if inner.accounts.contains_key(&username) {
        return Err(ApiError::conflict("账户名称已存在，请直接登录"));
    }
    inner.accounts.insert(username.clone(), Account { password: password_digest(&request.password) });
    let response = issue_auth(&mut inner, username);
    persist_inner(&state.data_path, &inner).map_err(ApiError::internal)?;
    Ok(Json(response))
}

fn issue_auth(inner: &mut Inner, username: String) -> AuthResponse {
    let now = now_unix();
    inner.tokens.retain(|_, (_, deadline)| *deadline > now);
    inner.refresh_grants.retain(|_, grant| grant.expires_at > now);
    let token = starbridge_protocol::new_token();
    let refresh_token = starbridge_protocol::new_token();
    let expires_at = now + 86_400;
    let refresh_expires_at = now + 30 * 86_400;
    inner.tokens.insert(token.clone(), (username.clone(), expires_at));
    inner.refresh_grants.insert(password_digest(&refresh_token), RefreshGrant {
        username: username.clone(), expires_at: refresh_expires_at, access_token_hash: password_digest(&token),
    });
    AuthResponse {
        access_token: token,
        username,
        expires_at,
        refresh_token: Some(refresh_token),
        refresh_expires_at: Some(refresh_expires_at),
    }
}

async fn refresh_auth(
    State(state): State<AppState>,
    Json(request): Json<AuthRefreshRequest>,
) -> Result<Json<AuthResponse>, ApiError> {
    let mut inner = state.inner.write().await;
    let key = password_digest(&request.refresh_token);
    let grant = inner.refresh_grants.get(&key).cloned()
        .filter(|grant| grant.expires_at > now_unix() && inner.accounts.contains_key(&grant.username))
        .ok_or_else(|| ApiError::unauthorized("登录状态已失效，请重新登录"))?;
    // Rotation and old-access revocation occur under the same write lock.
    inner.refresh_grants.remove(&key);
    inner.tokens.retain(|token, _| password_digest(token) != grant.access_token_hash);
    let response = issue_auth(&mut inner, grant.username);
    persist_inner(&state.data_path, &inner).map_err(ApiError::internal)?;
    Ok(Json(response))
}

async fn logout_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AuthLogoutRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut inner = state.inner.write().await;
    if let Some(token) = request.refresh_token {
        if let Some(grant) = inner.refresh_grants.remove(&password_digest(&token)) {
            inner.tokens.retain(|access, _| password_digest(access) != grant.access_token_hash);
        }
    }
    if let Some(access) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) {
        inner.tokens.remove(access);
        let digest = password_digest(access);
        inner.refresh_grants.retain(|_, grant| grant.access_token_hash != digest);
    }
    persist_inner(&state.data_path, &inner).map_err(ApiError::internal)?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn register_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(registration): Json<DeviceRegistration>,
) -> Result<Json<Vec<DeviceInfo>>, ApiError> {
    // Preserve the legacy response shape; old clients cannot adopt a different ID.
    let response = register_identified(&state, &headers, registration, false, None).await?;
    Ok(Json(response.devices))
}

async fn identify_device(
    State(state): State<AppState>, headers: HeaderMap, Json(registration): Json<DeviceRegistration>,
) -> Result<Json<DeviceRegistrationResponse>, ApiError> {
    Ok(Json(register_identified(&state, &headers, registration, true, None).await?))
}

async fn rebind_device(
    State(state): State<AppState>, headers: HeaderMap, Json(request): Json<DeviceRebindRequest>,
) -> Result<Json<DeviceRegistrationResponse>, ApiError> {
    if request.registration.machine_key.is_none() {
        return Err(ApiError::bad_request("请先完成本机身份初始化"));
    }
    Ok(Json(register_identified(&state, &headers, request.registration, true, Some(request.auth)).await?))
}

async fn register_identified(
    state: &AppState, headers: &HeaderMap, registration: DeviceRegistration,
    allow_canonical_id: bool, rebind_auth: Option<AuthRequest>,
) -> Result<DeviceRegistrationResponse, ApiError> {
    let account = authenticate(&state, &headers).await?;
    validate_registration(&registration)?;
    let mut inner = state.inner.write().await;
    if let Some(auth) = &rebind_auth {
        if auth.username.trim() != account || !inner.accounts.get(&account)
            .is_some_and(|stored| stored.password == password_digest(&auth.password)) {
            return Err(ApiError::unauthorized("重新绑定需要验证当前账户密码"));
        }
    }
    // Uniqueness, replacement and persistence share the same write lock.
    let previous_devices = inner.devices.clone();
    let previous_deleted = inner.deleted_devices.clone();
    let node_id = store_registration(&mut inner, &account, registration, allow_canonical_id, rebind_auth.is_some())?;
    if let Err(error) = persist_inner(&state.data_path, &inner) {
        inner.devices = previous_devices;
        inner.deleted_devices = previous_deleted;
        return Err(ApiError::internal(error));
    }
    Ok(DeviceRegistrationResponse { node_id, devices: list_devices(&inner, Some(&account)) })
}

fn store_registration(
    inner: &mut Inner, account: &str, mut registration: DeviceRegistration,
    allow_canonical_id: bool, explicit_rebind: bool,
) -> Result<String, ApiError> {
    if registration.direct_only && registration.allow_relay {
        return Err(ApiError::bad_request("只允许直连的节点不能同时允许中继"));
    }
    let requested_id = registration.node_id.clone();
    let mut matches = HashSet::new();
    let mut deleted_matches = HashSet::new();
    if let Some(key) = &registration.machine_key {
        for (id, device) in &inner.devices {
            if device.account == account && device.registration.machine_key.as_ref() == Some(key) {
                matches.insert(id.clone());
            }
        }
        for (id, deleted) in &inner.deleted_devices {
            if deleted.account == account && deleted.machine_key.as_ref() == Some(key) {
                // Deleted duplicate records keep their revocations even after the
                // user resolves the duplicate by retaining just one live record.
                if deleted.revoked_token_hashes.contains(&password_digest(&registration.device_token)) {
                    return Err(ApiError::new(StatusCode::GONE, "此设备凭据已被撤销，请验证账户后明确重新添加"));
                }
                deleted_matches.insert(id.clone());
            }
        }
    }
    if matches.len() > 1 {
        return Err(ApiError::conflict("此电脑存在多个历史设备记录，请在设备列表核对并删除多余记录后重试"));
    }
    let requested_belongs_elsewhere = inner.devices.get(&requested_id).is_some_and(|device| device.account != account)
        || inner.deleted_devices.get(&requested_id).is_some_and(|device| device.account != account);
    if matches.is_empty() && deleted_matches.len() > 1 && !explicit_rebind {
        return Err(ApiError::conflict("此电脑的多个历史记录均已删除，请验证账户后明确重新添加"));
    }
    let canonical_id = matches.into_iter().next().or_else(|| {
        if deleted_matches.len() == 1 { deleted_matches.into_iter().next() } else { None }
    }).unwrap_or_else(|| {
        if requested_belongs_elsewhere && allow_canonical_id && registration.machine_key.is_some() { new_id() }
        else { requested_id.clone() }
    });
    if requested_belongs_elsewhere && canonical_id == requested_id {
        return Err(ApiError::conflict("设备身份属于其他账户，请使用当前账户的独立身份"));
    }
    if canonical_id != requested_id {
        if !allow_canonical_id {
            return Err(ApiError::conflict("请升级客户端以复用已有设备记录"));
        }
        if inner.devices.get(&requested_id).is_some_and(|device| device.account == account) {
            return Err(ApiError::conflict("本机与历史记录存在重复身份，请核对旧数据并删除多余记录后重试"));
        }
    }
    ensure_device_not_deleted(inner, account, &canonical_id, &registration.device_token)?;
    let metadata_edited = if let Some(existing) = inner.devices.get(&canonical_id).cloned() {
        if existing.account != account {
            return Err(ApiError::conflict("设备身份属于其他账户"));
        }
        if existing.registration.machine_key.is_some() && existing.registration.machine_key != registration.machine_key {
            return Err(ApiError::conflict("本机识别来源与原设备不一致，请恢复原身份数据或使用独立测试目录"));
        }
        if existing.device_token != registration.device_token {
            if existing.registration.machine_key.is_none() {
                return Err(ApiError::conflict("迁移旧设备需要原设备凭据，请从原数据目录导入身份后重试"));
            }
            if !explicit_rebind {
                return Err(ApiError::conflict("此电脑已有设备但凭据不同，请验证账户后明确重新绑定；克隆电脑请使用独立测试目录"));
            }
            if diagnostics::occupied(inner, &canonical_id) || inner.sessions.values().any(|session| is_active(session)
                && (session.initiator_node_id == canonical_id || session.target_node_id == canonical_id)) {
                return Err(ApiError::conflict("设备正在连接或测试，请先终止相关任务后重新绑定"));
            }
            inner.deleted_devices.entry(canonical_id.clone()).or_insert_with(|| DeletedDevice {
                account: account.into(), revoked_token_hashes: HashSet::new(),
                machine_key: existing.registration.machine_key.clone(),
                display_name: existing.registration.display_name.clone(),
            }).revoked_token_hashes.insert(password_digest(&existing.device_token));
        }
        if existing.metadata_edited || canonical_id != requested_id || explicit_rebind {
            registration.display_name = existing.registration.display_name.clone();
            registration.purpose = existing.registration.purpose.clone();
        }
        existing.metadata_edited
    } else {
        if let Some(deleted) = inner.deleted_devices.get(&canonical_id) {
            if !explicit_rebind {
                return Err(ApiError::new(StatusCode::GONE, "此设备已被删除，请验证账户后明确重新添加"));
            }
            if deleted.machine_key.is_some() && deleted.machine_key != registration.machine_key {
                return Err(ApiError::conflict("本机识别来源与已删除设备不一致，请核对身份数据"));
            }
        }
        false
    };
    registration.node_id = canonical_id.clone();
    inner.devices.insert(
        canonical_id.clone(),
        StoredDevice {
            account: account.into(),
            device_token: registration.device_token.clone(),
            registration,
            last_heartbeat: now_unix(),
            status: "online".to_string(),
            metadata_edited,
        },
    );
    Ok(canonical_id)
}

async fn update_device_metadata(
    State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>,
    Json(request): Json<DeviceMetadataUpdate>,
) -> Result<Json<Vec<DeviceInfo>>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    if request.display_name.as_ref().is_some_and(|name| name.trim().is_empty() || name.chars().count() > 80)
        || request.purpose.as_ref().is_some_and(|purpose| purpose.chars().count() > 240) {
        return Err(ApiError::bad_request("设备名称不能为空且最多80字，用途最多240字"));
    }
    let mut inner = state.inner.write().await;
    let device = inner.devices.get_mut(&id).ok_or_else(|| ApiError::not_found("设备不存在"))?;
    if device.account != account { return Err(ApiError::forbidden("不能修改其他账户的设备")); }
    if let Some(name) = request.display_name { device.registration.display_name = name.trim().into(); }
    if let Some(purpose) = request.purpose { device.registration.purpose = purpose.trim().into(); }
    device.metadata_edited = true;
    persist_inner(&state.data_path, &inner).map_err(ApiError::internal)?;
    Ok(Json(list_devices(&inner, Some(&account))))
}

async fn delete_device(
    State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>,
) -> Result<Json<Vec<DeviceInfo>>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let mut inner = state.inner.write().await;
    let requester = headers.get("x-starbridge-node-id")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("缺少发起设备身份"))?;
    authenticate_device(&inner, &account, requester, device_token_header(&headers)?)?;
    let device = inner.devices.get(&id).cloned()
        .ok_or_else(|| ApiError::not_found("设备不存在"))?;
    if device.account != account { return Err(ApiError::forbidden("不能删除其他账户的设备")); }
    expire_sessions(&mut inner, now_unix());
    diagnostics::expire(&mut inner, now_unix());
    if diagnostics::occupied(&inner, &id) || inner.sessions.values().any(|session| is_active(session)
        && (session.initiator_node_id == id || session.target_node_id == id)) {
        return Err(ApiError::conflict("设备正在连接或测试，请先终止相关任务后再删除"));
    }
    let previous_tombstone = inner.deleted_devices.get(&id).cloned();
    inner.deleted_devices.entry(id.clone()).or_insert_with(|| DeletedDevice {
        account: account.clone(), revoked_token_hashes: HashSet::new(), machine_key: None,
        display_name: String::new(),
    });
    let tombstone = inner.deleted_devices.get_mut(&id).unwrap();
    tombstone.machine_key = device.registration.machine_key.clone();
    tombstone.display_name = device.registration.display_name.clone();
    tombstone.revoked_token_hashes.insert(password_digest(&device.device_token));
    inner.devices.remove(&id);
    if let Err(error) = persist_inner(&state.data_path, &inner) {
        inner.devices.insert(id.clone(), device);
        if let Some(tombstone) = previous_tombstone { inner.deleted_devices.insert(id, tombstone); }
        else { inner.deleted_devices.remove(&id); }
        return Err(ApiError::internal(error));
    }
    Ok(Json(list_devices(&inner, Some(&account))))
}

async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<HeartbeatRequest>,
) -> Result<Json<Vec<DeviceInfo>>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let mut inner = state.inner.write().await;
    ensure_device_not_deleted(&inner, &account, &request.node_id, &request.device_token)?;
    let device = inner
        .devices
        .get_mut(&request.node_id)
        .ok_or_else(|| ApiError::not_found("设备尚未登记"))?;
    if device.account != account || device.device_token != request.device_token {
        return Err(ApiError::forbidden("设备身份凭据不正确"));
    }
    device.last_heartbeat = now_unix();
    device.status = request.status;
    expire_sessions(&mut inner, now_unix());
    diagnostics::expire(&mut inner, now_unix());
    Ok(Json(list_devices(&inner, Some(&account))))
}

async fn devices(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<DeviceInfo>>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let mut inner = state.inner.write().await;
    expire_sessions(&mut inner, now_unix());
    diagnostics::expire(&mut inner, now_unix());
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
            device_ids.contains(&session.initiator_node_id)
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
    expire_sessions(&mut inner, now_unix());
    diagnostics::expire(&mut inner, now_unix());
    let initiator = authenticate_device(
        &inner,
        &account,
        &request.initiator_node_id,
        &request.device_token,
    )?;
    if !is_online(initiator) {
        return Err(ApiError::conflict("发起节点当前离线"));
    }
    if !has_capability(&initiator.registration, "ssh_client") {
        return Err(ApiError::conflict("发起节点不支持 SSH 连接"));
    }
    if request.initiator_node_id == request.target_node_id {
        return Err(ApiError::bad_request("不能连接本机节点"));
    }
    let initiator_registration = initiator.registration.clone();
    let target = inner
        .devices
        .get(&request.target_node_id)
        .ok_or_else(|| ApiError::not_found("目标节点不存在"))?;
    if target.account != account {
        return Err(ApiError::forbidden("不能连接其他账户的节点"));
    }
    if !is_online(target) {
        return Err(ApiError::conflict("目标节点当前离线"));
    }
    if !has_capability(&target.registration, "ssh_server") {
        return Err(ApiError::conflict("目标节点尚未允许 SSH 访问或 SSH 服务未就绪"));
    }
    let target_registration = target.registration.clone();
    if let Some(existing) = inner.sessions.values().find(|session| is_active(session)
        && session.initiator_node_id == request.initiator_node_id && session.target_node_id == request.target_node_id) {
        return Ok(Json(existing.clone()));
    }
    if diagnostics::occupied(&inner, &request.initiator_node_id) || diagnostics::occupied(&inner, &request.target_node_id) {
        return Err(ApiError::conflict("设备正在连接测试，请等待测试结束"));
    }
    if inner.sessions.values().any(|session| is_active(session) && session.target_node_id == request.initiator_node_id) {
        return Err(ApiError::conflict("本机正在被其他设备连接，不能发起新连接"));
    }
    if inner.sessions.values().any(|session| is_active(session)
        && (session.target_node_id == request.target_node_id || session.initiator_node_id == request.target_node_id)) {
        return Err(ApiError::conflict("目标设备已参与其他连接，当前被占用"));
    }
    let policy = effective_policy(request.policy, &initiator_registration, &target_registration)?;
    let created_at = now_unix();
    let session_id = new_id();
    let proxy_name = format!(
        "sb-{}-{}",
        &target_registration.node_id[..8.min(target_registration.node_id.len())],
        &session_id[..8]
    );
    let info = SessionInfo {
        session_id,
        initiator_node_id: initiator_registration.node_id,
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

async fn session_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(node_id): Path<String>,
    Query(query): Query<SessionHistoryQuery>,
) -> Result<Json<SessionHistoryPage>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let mut inner = state.inner.write().await;
    authenticate_device(&inner, &account, &node_id, device_token_header(&headers)?)?;
    let changed = expire_sessions(&mut inner, now_unix());
    let mut sessions: Vec<_> = inner.sessions.values().filter(|session| {
        session.initiator_node_id == node_id || session.target_node_id == node_id
    }).collect();
    sessions.sort_by(|left, right| {
        right.created_at.cmp(&left.created_at)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    let total = sessions.len();
    let items = sessions.into_iter().skip(query.offset).take(50).map(|session| {
        let outgoing = session.initiator_node_id == node_id;
        let peer = if outgoing { &session.target_node_id } else { &session.initiator_node_id };
        let peer_name = inner.devices.get(peer).filter(|device| device.account == account)
            .map(|device| device.registration.display_name.clone())
            .or_else(|| inner.deleted_devices.get(peer).filter(|device| device.account == account)
                .map(|device| device.display_name.clone()))
            .filter(|name| !name.is_empty()).unwrap_or_else(|| "已删除的设备".into());
        SessionHistoryEntry {
            session_id: session.session_id.clone(), peer_name,
            direction: if outgoing { "outgoing" } else { "incoming" }.into(),
            status: session.status, path: session.path, created_at: session.created_at,
            failure_reason: session.failure_reason.clone(),
        }
    }).collect();
    drop(inner);
    if changed { state.persist().await; }
    Ok(Json(SessionHistoryPage { items, total }))
}

async fn assigned_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(node_id): Path<String>,
) -> Result<Json<Vec<SessionInfo>>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let mut inner = state.inner.write().await;
    authenticate_device(&inner, &account, &node_id, device_token_header(&headers)?)?;
    let changed = expire_sessions(&mut inner, now_unix());
    let mut sessions: Vec<_> = inner
        .sessions
        .values()
        .filter(|session| {
            (session.initiator_node_id == node_id || session.target_node_id == node_id)
                && is_active(session)
        })
        .cloned()
        .collect();
    sessions.sort_by(|left, right| {
        left.created_at.cmp(&right.created_at)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    drop(inner);
    if changed {
        state.persist().await;
    }
    Ok(Json(sessions))
}

async fn get_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<SessionInfo>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let mut inner = state.inner.write().await;
    expire_sessions(&mut inner, now_unix());
    let session = inner
        .sessions
        .get(&id)
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    ensure_session_device(&inner, session, &account, &headers)?;
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
    expire_sessions(&mut inner, now_unix());
    let existing = inner
        .sessions
        .get(&id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    ensure_session_device(&inner, &existing, &account, &headers)?;
    if !is_active(&existing) {
        return Err(ApiError::conflict("会话已结束，不能更新状态"));
    }
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
    ensure_session_device(&inner, &existing, &account, &headers)?;
    let session = inner.sessions.get_mut(&id).expect("session exists");
    session.status = SessionStatus::Closed;
    let response = Json(session.clone());
    drop(inner);
    state.persist().await;
    Ok(response)
}

fn validate_registration(registration: &DeviceRegistration) -> Result<(), ApiError> {
    if registration.machine_key.as_ref().is_some_and(|key| key.len() != 64
        || !key.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))) {
        return Err(ApiError::bad_request("本机身份数据格式错误，请检查数据目录"));
    }
    if registration.node_id.trim().is_empty()
        || registration.node_id.len() > 128
        || registration.device_token.trim().is_empty()
        || registration.display_name.trim().is_empty()
        || registration.display_name.chars().count() > 80
        || registration.purpose.chars().count() > 240
        || registration.ssh_alias.trim().is_empty()
    {
        return Err(ApiError::bad_request("设备身份或显示名称格式错误"));
    }
    if [&registration.ssh_public_key, &registration.ssh_host_key]
        .iter()
        .any(|key| key.len() > 1024 || key.contains(['\r', '\n']))
    {
        return Err(ApiError::bad_request("SSH 公钥格式错误"));
    }
    Ok(())
}

fn effective_policy(
    requested: ConnectionPolicy,
    initiator: &DeviceRegistration,
    target: &DeviceRegistration,
) -> Result<ConnectionPolicy, ApiError> {
    if requested == ConnectionPolicy::RelayOnly
        && (initiator.direct_only
            || target.direct_only
            || !initiator.allow_relay
            || !target.allow_relay)
    {
        return Err(ApiError::conflict("节点策略禁止使用中继"));
    }
    if requested == ConnectionPolicy::DirectPreferred
        && (initiator.direct_only || target.direct_only)
    {
        return Ok(ConnectionPolicy::DirectOnly);
    }
    if requested != ConnectionPolicy::DirectOnly && (!initiator.allow_relay || !target.allow_relay)
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
            purpose: device.registration.purpose.clone(),
            occupancy: occupancy(inner, &device.registration.node_id).0,
            occupied_by: occupancy(inner, &device.registration.node_id).1,
            outgoing_count: inner.sessions.values().filter(|session| is_active(session) && session.initiator_node_id == device.registration.node_id).count(),
            role: device.registration.role,
            ssh_alias: device.registration.ssh_alias.clone(),
            ssh_user: device.registration.ssh_user.clone(),
            ssh_privilege: device.registration.ssh_privilege,
            ssh_public_key: device.registration.ssh_public_key.clone(),
            ssh_host_key: device.registration.ssh_host_key.clone(),
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

fn occupancy(inner: &Inner, node: &str) -> (String, Option<String>) {
    if diagnostics::occupied(inner, node) { return ("diagnostic".into(), None); }
    if let Some(session) = inner.sessions.values().find(|session| is_active(session) && session.target_node_id == node) {
        return ("incoming".into(), Some(session.initiator_node_id.clone()));
    }
    if inner.sessions.values().any(|session| is_active(session) && session.initiator_node_id == node) {
        return ("outgoing".into(), None);
    }
    ("idle".into(), None)
}

fn is_online(device: &StoredDevice) -> bool {
    device.status == "online" && now_unix().saturating_sub(device.last_heartbeat) <= 30
}

fn has_capability(device: &DeviceRegistration, capability: &str) -> bool {
    device.capabilities.iter().any(|value| value == capability)
}

fn is_active(session: &SessionInfo) -> bool {
    !matches!(session.status, SessionStatus::Closed | SessionStatus::Failed)
}

/// 协商有时限；已建立的长会话由双方心跳维持，不按创建时间截断。
fn expire_sessions(inner: &mut Inner, now: u64) -> bool {
    let mut changed = false;
    for session in inner.sessions.values_mut().filter(|session| is_active(session)) {
        let endpoints_online = [&session.initiator_node_id, &session.target_node_id]
            .iter()
            .all(|node_id| {
                inner.devices.get(*node_id).is_some_and(|device| {
                    device.status == "online" && now.saturating_sub(device.last_heartbeat) <= 30
                })
            });
        if !endpoints_online {
            session.status = SessionStatus::Closed;
            session.failure_reason = Some("会话节点已离线".into());
            changed = true;
        } else if session.status == SessionStatus::Negotiating && session.expires_at <= now {
            session.status = SessionStatus::Failed;
            session.failure_reason = Some("连接协商已过期".into());
            changed = true;
        }
    }
    changed
}

// Keep revoked credentials after re-registration so an old background client cannot restore a deleted node.
fn ensure_device_not_deleted(inner: &Inner, account: &str, node_id: &str, token: &str) -> Result<(), ApiError> {
    if let Some(deleted) = inner.deleted_devices.get(node_id) {
        if deleted.account != account { return Err(ApiError::forbidden("设备身份属于其他账户")); }
        if deleted.revoked_token_hashes.contains(&password_digest(token)) {
            return Err(ApiError::new(StatusCode::GONE, "此设备凭据已撤销，请登录账号并明确重新绑定"));
        }
    }
    Ok(())
}

fn authenticate_device<'a>(
    inner: &'a Inner,
    account: &str,
    node_id: &str,
    token: &str,
) -> Result<&'a StoredDevice, ApiError> {
    ensure_device_not_deleted(inner, account, node_id, token)?;
    let device = inner.devices.get(node_id)
        .ok_or_else(|| ApiError::not_found("设备尚未登记"))?;
    if device.account != account || token.is_empty() || device.device_token != token {
        return Err(ApiError::forbidden("设备身份凭据不正确"));
    }
    Ok(device)
}

fn device_token_header(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers.get("x-starbridge-device-token")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::forbidden("缺少设备身份凭据"))
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

fn ensure_session_device(
    inner: &Inner,
    session: &SessionInfo,
    account: &str,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    let node_id = headers.get("x-starbridge-node-id")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("缺少发起设备身份"))?;
    authenticate_device(inner, account, node_id, device_token_header(headers)?)?;
    if node_id != session.initiator_node_id && node_id != session.target_node_id {
        return Err(ApiError::forbidden("本机不是这个会话的参与节点"));
    }
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
    fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }
    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "保存服务状态失败");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "保存服务状态失败，请稍后重试")
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
    use starbridge_protocol::{Role, SshPrivilege};

    pub(super) fn device(direct_only: bool, allow_relay: bool) -> DeviceRegistration {
        DeviceRegistration {
            node_id: "node".into(),
            device_token: "token".into(),
            machine_key: None,
            display_name: "node".into(),
            purpose: String::new(),
            role: Role::Controller,
            ssh_alias: "node".into(),
            ssh_user: "user".into(),
            ssh_privilege: SshPrivilege::User,
            ssh_public_key: String::new(),
            ssh_host_key: String::new(),
            capabilities: vec!["ssh".into()],
            allow_relay,
            direct_only,
            version: "test".into(),
            os: "windows".into(),
            arch: "x86_64".into(),
            hostname: "node".into(),
        }
    }

    pub(super) struct TestApp {
        pub state: AppState,
    }

    impl Drop for TestApp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.state.data_path.as_ref());
        }
    }

    pub(super) fn test_app() -> TestApp {
        let directory = std::env::temp_dir().join(format!("starbridge-peer-test-{}", new_id()));
        let state = AppState::with_frp_and_data("localhost".into(), 17851, "frp-token".into(), directory);
        {
            let mut inner = state.inner.try_write().unwrap();
            for account in ["alice", "bob"] {
                inner.accounts.insert(account.into(), Account { password: password_digest("test1234") });
                inner.tokens.insert(format!("{account}-access"), (account.into(), now_unix() + 3600));
            }
            for (node_id, account) in [("a", "alice"), ("b", "alice"), ("c", "alice"), ("d", "alice"), ("z", "bob")] {
                let mut registration = device(false, true);
                registration.node_id = node_id.into();
                registration.device_token = format!("{node_id}-token");
                registration.role = Role::Peer;
                registration.ssh_public_key = format!("ssh-ed25519 {node_id}-public");
                registration.ssh_host_key = format!("ssh-ed25519 {node_id}-host");
                registration.capabilities = vec!["ssh_client".into(), "ssh_server".into(), "ssh".into()];
                inner.devices.insert(node_id.into(), StoredDevice {
                    account: account.into(),
                    device_token: registration.device_token.clone(),
                    registration,
                    last_heartbeat: now_unix(),
                    status: "online".into(),
                    metadata_edited: false,
                });
            }
        }
        TestApp { state }
    }

    pub(super) fn peer_headers(node_id: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer alice-access".parse().unwrap());
        headers.insert("x-starbridge-node-id", node_id.parse().unwrap());
        headers.insert("x-starbridge-device-token", format!("{node_id}-token").parse().unwrap());
        headers
    }

    pub(super) fn session_request(initiator: &str, target: &str) -> SessionCreateRequest {
        SessionCreateRequest {
            initiator_node_id: initiator.into(),
            device_token: format!("{initiator}-token"),
            target_node_id: target.into(),
            policy: ConnectionPolicy::RelayOnly,
        }
    }

    async fn create_peer_session(state: &AppState, initiator: &str, target: &str) -> SessionInfo {
        create_session(State(state.clone()), peer_headers(initiator), Json(session_request(initiator, target)))
            .await.unwrap().0
    }

    async fn history_page(state: &AppState, node: &str, offset: usize) -> SessionHistoryPage {
        session_history(State(state.clone()), peer_headers(node), Path(node.into()),
            Query(SessionHistoryQuery { offset })).await.unwrap().0
    }

    #[tokio::test]
    async fn session_history_keeps_repeated_connections_in_both_directions_and_deleted_peer_names() {
        let app = test_app();
        app.state.inner.write().await.devices.get_mut("b").unwrap().registration.display_name = "开发电脑".into();
        let outgoing = create_peer_session(&app.state, "a", "b").await;
        let _ = delete_session(State(app.state.clone()), peer_headers("a"), Path(outgoing.session_id.clone())).await.unwrap();
        let incoming = create_peer_session(&app.state, "b", "a").await;
        let _ = delete_session(State(app.state.clone()), peer_headers("b"), Path(incoming.session_id.clone())).await.unwrap();
        create_peer_session(&app.state, "c", "d").await;
        let page = history_page(&app.state, "a", 0).await;
        assert_eq!(page.total, 2);
        assert!(page.items.iter().any(|item| item.session_id == outgoing.session_id && item.direction == "outgoing"));
        assert!(page.items.iter().any(|item| item.session_id == incoming.session_id && item.direction == "incoming"));
        assert!(page.items.iter().all(|item| item.peer_name == "开发电脑"));
        let json = serde_json::to_value(&page).unwrap();
        assert!(json["items"].as_array().unwrap().iter().all(|item| item.as_object().unwrap().len() == 7));
        let text = json.to_string();
        for secret in ["frp-token", "secret_key", "route", "ssh_command", "device_token"] {
            assert!(!text.contains(secret), "history exposed {secret}");
        }
        let _ = delete_device(State(app.state.clone()), peer_headers("a"), Path("b".into())).await.unwrap();
        assert!(history_page(&app.state, "a", 0).await.items.iter().all(|item| item.peer_name == "开发电脑"));
        app.state.inner.write().await.deleted_devices.get_mut("b").unwrap().display_name.clear();
        assert!(history_page(&app.state, "a", 0).await.items.iter().all(|item| item.peer_name == "已删除的设备"));
    }

    #[tokio::test]
    async fn session_history_requires_account_and_device_authentication_and_routes_default_offset() {
        let app = test_app();
        create_peer_session(&app.state, "a", "b").await;
        let error = session_history(State(app.state.clone()), HeaderMap::new(), Path("a".into()),
            Query(SessionHistoryQuery::default())).await.unwrap_err();
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);
        let mut missing = peer_headers("a");
        missing.remove("x-starbridge-device-token");
        for (headers, node) in [(missing, "a"), (peer_headers("b"), "a"), (peer_headers("z"), "z")] {
            let error = session_history(State(app.state.clone()), headers, Path(node.into()),
                Query(SessionHistoryQuery::default())).await.unwrap_err();
            assert_eq!(error.status, StatusCode::FORBIDDEN);
        }
        assert_eq!(history_page(&app.state, "c", 0).await.total, 0);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = super::app(app.state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        let response = reqwest::Client::new().get(format!("http://{address}/api/v1/sessions/history/a"))
            .headers(peer_headers("a")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.json::<SessionHistoryPage>().await.unwrap().total, 1);
        server.abort();
    }

    #[tokio::test]
    async fn session_history_paginates_with_stable_newest_first_order() {
        let app = test_app();
        let template = create_peer_session(&app.state, "a", "b").await;
        {
            let mut inner = app.state.inner.write().await;
            inner.sessions.clear();
            for index in 0..53 {
                let mut session = template.clone();
                session.session_id = format!("history-{index:02}");
                session.created_at = 100 + index / 2;
                session.status = SessionStatus::Closed;
                inner.sessions.insert(session.session_id.clone(), session);
            }
        }
        let first = history_page(&app.state, "a", 0).await;
        let second = history_page(&app.state, "a", first.items.len()).await;
        assert_eq!(first.total, 53);
        assert_eq!(first.items.len(), 50);
        assert_eq!(second.total, 53);
        assert_eq!(second.items.len(), 3);
        let ids: Vec<_> = first.items.iter().chain(&second.items).map(|item| item.session_id.as_str()).collect();
        let mut expected: Vec<_> = (0..53).collect();
        expected.sort_by_key(|index| (std::cmp::Reverse(index / 2), *index));
        let expected: Vec<_> = expected.iter().map(|index| format!("history-{index:02}")).collect();
        assert_eq!(ids, expected);
        let empty = history_page(&app.state, "a", usize::MAX).await;
        assert_eq!(empty.total, 53);
        assert!(empty.items.is_empty());
    }

    #[tokio::test]
    async fn session_history_expires_and_persists_records_without_overwriting_past_results_on_restart() {
        let app = test_app();
        let template = create_peer_session(&app.state, "a", "b").await;
        {
            let mut inner = app.state.inner.write().await;
            inner.sessions.clear();
            for (id, target, status, reason) in [
                ("failed", "b", SessionStatus::Failed, Some("SSH 验证失败")),
                ("closed", "b", SessionStatus::Closed, Some("用户已结束连接")),
                ("connected", "c", SessionStatus::Connected, None),
                ("expired", "d", SessionStatus::Negotiating, None),
            ] {
                let mut session = template.clone();
                session.session_id = id.into(); session.target_node_id = target.into();
                session.status = status; session.failure_reason = reason.map(str::to_string);
                session.expires_at = 0;
                inner.sessions.insert(id.into(), session);
            }
            inner.devices.get_mut("b").unwrap().registration.display_name = "历史目标电脑".into();
        }
        let page = history_page(&app.state, "a", 0).await;
        assert_eq!(page.items.iter().find(|item| item.session_id == "expired").unwrap().status, SessionStatus::Failed);
        assert_eq!(load_persisted(&app.state.data_path).unwrap().sessions["expired"].status, SessionStatus::Failed);
        let _ = delete_device(State(app.state.clone()), peer_headers("a"), Path("b".into())).await.unwrap();
        let restored = AppState::with_frp_and_data("localhost".into(), 17851, "frp-token".into(), app.state.data_path.as_ref().clone());
        let auth = login(State(restored.clone()), Json(AuthRequest { username: "alice".into(), password: "test1234".into() })).await.unwrap().0;
        let mut headers = peer_headers("a");
        headers.insert(header::AUTHORIZATION, format!("Bearer {}", auth.access_token).parse().unwrap());
        let page = session_history(State(restored), headers, Path("a".into()), Query(SessionHistoryQuery::default())).await.unwrap().0;
        assert_eq!(page.total, 4);
        for (id, expected) in [("failed", "SSH 验证失败"), ("closed", "用户已结束连接"),
            ("expired", "连接协商已过期"), ("connected", "中心服务重启，会话已结束")] {
            let item = page.items.iter().find(|item| item.session_id == id).unwrap();
            assert_eq!(item.failure_reason.as_deref(), Some(expected));
            if id != "expired" && id != "connected" { assert_eq!(item.peer_name, "历史目标电脑"); }
        }
    }

    #[tokio::test]
    async fn one_to_many_admission_reuses_same_direction_and_rejects_mixed_roles() {
        let app = test_app();
        let first = create_peer_session(&app.state, "a", "b").await;
        let duplicate = create_peer_session(&app.state, "a", "b").await;
        assert_eq!(duplicate.session_id, first.session_id);
        assert_eq!(duplicate.route.as_ref().unwrap().secret_key, first.route.as_ref().unwrap().secret_key);
        for (from, to) in [("b", "a"), ("b", "c"), ("c", "a"), ("c", "b"), ("d", "a"), ("d", "b")] {
            assert_eq!(create_session(State(app.state.clone()), peer_headers(from), Json(session_request(from, to)))
                .await.unwrap_err().status, StatusCode::CONFLICT, "{from}->{to}");
        }
        create_peer_session(&app.state, "a", "c").await;
        create_peer_session(&app.state, "a", "d").await;
        let inner = app.state.inner.read().await;
        assert_eq!(inner.sessions.values().filter(|s| is_active(s)).count(), 3);
        assert_eq!(occupancy(&inner, "a").0, "outgoing");
        assert_eq!(occupancy(&inner, "b"), ("incoming".into(), Some("a".into())));
    }

    #[tokio::test]
    async fn create_checks_device_identity_and_capabilities_instead_of_legacy_roles() {
        let app = test_app();
        {
            let mut inner = app.state.inner.write().await;
            inner.devices.get_mut("a").unwrap().registration.role = Role::Executor;
            inner.devices.get_mut("b").unwrap().registration.role = Role::Controller;
            // 外部 SSH 服务可以没有预登记的主机公钥。
            inner.devices.get_mut("b").unwrap().registration.ssh_host_key.clear();
        }
        let info = create_peer_session(&app.state, "a", "b").await;
        assert_eq!(info.initiator_node_id, "a");
        let mut wrong_identity = session_request("a", "b");
        wrong_identity.device_token = "b-token".into();
        let error = create_session(State(app.state.clone()), peer_headers("a"), Json(wrong_identity))
            .await.unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        for (initiator, target, expected) in [
            ("a", "a", StatusCode::BAD_REQUEST),
            ("a", "z", StatusCode::FORBIDDEN),
            ("z", "a", StatusCode::FORBIDDEN),
        ] {
            let error = create_session(State(app.state.clone()), peer_headers(initiator), Json(session_request(initiator, target)))
                .await.unwrap_err();
            assert_eq!(error.status, expected);
        }
        app.state.inner.write().await.devices.get_mut("b").unwrap().registration.capabilities.clear();
        assert_eq!(create_session(State(app.state.clone()), peer_headers("a"), Json(session_request("a", "b")))
            .await.unwrap_err().status, StatusCode::CONFLICT);
        app.state.inner.write().await.devices.get_mut("c").unwrap().registration.ssh_public_key.clear();
        assert_eq!(create_session(State(app.state.clone()), peer_headers("c"), Json(session_request("c", "a")))
            .await.unwrap_err().status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn assigned_lists_outgoing_and_incoming_views_and_requires_device_token() {
        let app = test_app();
        let outgoing = create_peer_session(&app.state, "a", "b").await;
        let second = create_peer_session(&app.state, "a", "c").await;
        {
            let mut inner = app.state.inner.write().await;
            inner.sessions.get_mut(&outgoing.session_id).unwrap().created_at = 2;
            inner.sessions.get_mut(&second.session_id).unwrap().created_at = 1;
        }
        let sessions = assigned_sessions(State(app.state.clone()), peer_headers("a"), Path("a".into()))
            .await.unwrap().0;
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, second.session_id);
        assert_eq!(sessions[1].session_id, outgoing.session_id);
        let incoming = assigned_sessions(State(app.state.clone()), peer_headers("b"), Path("b".into())).await.unwrap().0;
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].session_id, outgoing.session_id);
        let error = assigned_sessions(State(app.state.clone()), peer_headers("b"), Path("a".into()))
            .await.unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn assigned_expires_negotiation_and_offline_peers_but_keeps_long_connected_sessions() {
        let app = test_app();
        let long = create_peer_session(&app.state, "a", "b").await;
        let expired = create_peer_session(&app.state, "a", "c").await;
        {
            let mut inner = app.state.inner.write().await;
            let long_session = inner.sessions.get_mut(&long.session_id).unwrap();
            long_session.status = SessionStatus::Connected;
            long_session.expires_at = now_unix() - 1;
            inner.sessions.get_mut(&expired.session_id).unwrap().expires_at = now_unix() - 1;
        }
        let sessions = assigned_sessions(State(app.state.clone()), peer_headers("a"), Path("a".into()))
            .await.unwrap().0;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, long.session_id);
        assert_eq!(app.state.inner.read().await.sessions[&expired.session_id].status, SessionStatus::Failed);
        app.state.inner.write().await.devices.get_mut("b").unwrap().last_heartbeat = now_unix() - 31;
        assert!(assigned_sessions(State(app.state.clone()), peer_headers("a"), Path("a".into()))
            .await.unwrap().0.is_empty());
        assert_eq!(app.state.inner.read().await.sessions[&long.session_id].status, SessionStatus::Closed);
    }

    #[tokio::test]
    async fn session_updates_require_a_participating_device_and_cannot_revive_closed_sessions() {
        let app = test_app();
        let info = create_peer_session(&app.state, "a", "b").await;
        let error = delete_session(State(app.state.clone()), peer_headers("c"), Path(info.session_id.clone()))
            .await.unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        let _ = delete_session(State(app.state.clone()), peer_headers("b"), Path(info.session_id.clone())).await.unwrap();
        let error = update_session(State(app.state.clone()), peer_headers("a"), Path(info.session_id), Json(SessionStatusRequest {
            status: SessionStatus::Connected,
            path: SessionPath::Relay,
            failure_reason: None,
        })).await.unwrap_err();
        assert_eq!(error.status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn heartbeat_authenticates_device_and_respects_offline_status() {
        let app = test_app();
        let error = heartbeat(State(app.state.clone()), peer_headers("a"), Json(HeartbeatRequest {
            node_id: "a".into(), device_token: "wrong".into(), status: "online".into(),
        })).await.unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        let _ = heartbeat(State(app.state.clone()), peer_headers("a"), Json(HeartbeatRequest {
            node_id: "a".into(), device_token: "a-token".into(), status: "offline".into(),
        })).await.unwrap();
        assert!(!is_online(&app.state.inner.read().await.devices["a"]));
        assert_eq!(create_session(State(app.state.clone()), peer_headers("b"), Json(session_request("b", "a")))
            .await.unwrap_err().status, StatusCode::CONFLICT);
    }

    #[test]
    fn protocol_reads_legacy_session_direction_and_defaults_to_peer_role() {
        let legacy = serde_json::json!({
            "session_id": "legacy", "controller_node_id": "a", "target_node_id": "b",
            "policy": "relay_only", "path": "relay", "status": "connected",
            "created_at": 1, "expires_at": 2, "failure_reason": null, "route": null,
        });
        let session: SessionInfo = serde_json::from_value(legacy).unwrap();
        assert_eq!(session.initiator_node_id, "a");
        let serialized = serde_json::to_value(session).unwrap();
        assert_eq!(serialized["initiator_node_id"], "a");
        assert!(serialized.get("controller_node_id").is_none());
        let mut registration = serde_json::to_value(device(false, true)).unwrap();
        registration.as_object_mut().unwrap().remove("role");
        let registration: DeviceRegistration = serde_json::from_value(registration).unwrap();
        assert_eq!(registration.role, Role::Peer);
        assert_eq!(serde_json::from_str::<Role>("\"executor\"").unwrap(), Role::Executor);
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
                machine_key: None,
                display_name: node_id.into(),
                purpose: String::new(),
                role: Role::Executor,
                ssh_alias: node_id.into(),
                ssh_user: "user".into(),
                ssh_privilege: SshPrivilege::User,
                ssh_public_key: String::new(),
                ssh_host_key: String::new(),
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
                    metadata_edited: false,
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

    #[tokio::test]
    async fn concurrent_requests_cannot_both_reserve_target() {
        let app = test_app();
        let (first, second) = tokio::join!(
            create_session(State(app.state.clone()), peer_headers("a"), Json(session_request("a", "b"))),
            create_session(State(app.state.clone()), peer_headers("c"), Json(session_request("c", "b"))),
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert_eq!(first.err().or(second.err()).unwrap().status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn release_is_per_session_and_expired_updates_cannot_revive() {
        let app = test_app();
        let b = create_peer_session(&app.state, "a", "b").await;
        let c = create_peer_session(&app.state, "a", "c").await;
        let _ = delete_session(State(app.state.clone()), peer_headers("a"), Path(b.session_id)).await.unwrap();
        {
            let inner = app.state.inner.read().await;
            assert_eq!(occupancy(&inner, "b").0, "idle");
            assert_eq!(occupancy(&inner, "a").0, "outgoing");
        }
        app.state.inner.write().await.sessions.get_mut(&c.session_id).unwrap().expires_at = now_unix() - 1;
        assert_eq!(update_session(State(app.state.clone()), peer_headers("a"), Path(c.session_id), Json(SessionStatusRequest {
            status: SessionStatus::Connected, path: SessionPath::Relay, failure_reason: None,
        })).await.unwrap_err().status, StatusCode::CONFLICT);
        assert_eq!(occupancy(&*app.state.inner.read().await, "a").0, "idle");
        create_peer_session(&app.state, "d", "a").await;
    }

    #[tokio::test]
    async fn device_delete_persists_revocation_and_allows_explicit_new_credentials() {
        let app = test_app();
        let original = app.state.inner.read().await.devices["b"].registration.clone();
        let remaining = delete_device(State(app.state.clone()), peer_headers("a"), Path("b".into())).await.unwrap().0;
        assert!(remaining.iter().all(|device| device.node_id != "b" && device.account == "alice"));
        assert_eq!(delete_device(State(app.state.clone()), peer_headers("a"), Path("b".into())).await.unwrap_err().status, StatusCode::NOT_FOUND);
        assert_eq!(register_device(State(app.state.clone()), peer_headers("b"), Json(original.clone())).await.unwrap_err().status, StatusCode::GONE);
        assert_eq!(heartbeat(State(app.state.clone()), peer_headers("b"), Json(HeartbeatRequest {
            node_id: "b".into(), device_token: "b-token".into(), status: "online".into(),
        })).await.unwrap_err().status, StatusCode::GONE);
        let restored = AppState::with_frp_and_data("localhost".into(), 17851, "token".into(), app.state.data_path.as_ref().clone());
        {
            let inner = restored.inner.read().await;
            assert!(!inner.devices.contains_key("b"));
            assert_eq!(ensure_device_not_deleted(&inner, "alice", "b", "b-token").unwrap_err().status, StatusCode::GONE);
            assert_eq!(ensure_device_not_deleted(&inner, "bob", "b", "new-token").unwrap_err().status, StatusCode::FORBIDDEN);
        }
        let mut replacement = original.clone();
        replacement.device_token = "replacement-token".into();
        assert_eq!(register_device(State(app.state.clone()), peer_headers("a"), Json(replacement.clone())).await.unwrap_err().status, StatusCode::GONE);
        replacement.machine_key = Some("b".repeat(64));
        let _ = rebind_device(State(app.state.clone()), peer_headers("a"), Json(DeviceRebindRequest {
            registration: replacement,
            auth: AuthRequest { username: "alice".into(), password: "test1234".into() },
        })).await.unwrap();
        assert_eq!(register_device(State(app.state.clone()), peer_headers("b"), Json(original)).await.unwrap_err().status, StatusCode::GONE);
        let _ = delete_device(State(app.state.clone()), peer_headers("a"), Path("b".into())).await.unwrap();
        let inner = app.state.inner.read().await;
        assert_eq!(ensure_device_not_deleted(&inner, "alice", "b", "replacement-token").unwrap_err().status, StatusCode::GONE);
        assert_eq!(inner.deleted_devices["b"].revoked_token_hashes.len(), 2);
    }

    #[tokio::test]
    async fn device_delete_requires_requester_proof_same_account_and_idle_target() {
        let app = test_app();
        let mut missing_proof = peer_headers("a");
        missing_proof.remove("x-starbridge-device-token");
        assert_eq!(delete_device(State(app.state.clone()), missing_proof, Path("b".into())).await.unwrap_err().status, StatusCode::FORBIDDEN);
        assert_eq!(delete_device(State(app.state.clone()), peer_headers("z"), Path("b".into())).await.unwrap_err().status, StatusCode::FORBIDDEN);
        assert_eq!(delete_device(State(app.state.clone()), peer_headers("a"), Path("z".into())).await.unwrap_err().status, StatusCode::FORBIDDEN);
        let session = create_session(State(app.state.clone()), peer_headers("a"), Json(session_request("a", "b"))).await.unwrap().0;
        for node in ["a", "b"] {
            assert_eq!(delete_device(State(app.state.clone()), peer_headers("c"), Path(node.into())).await.unwrap_err().status, StatusCode::CONFLICT);
        }
        let _ = delete_session(State(app.state.clone()), peer_headers("a"), Path(session.session_id)).await.unwrap();
        let _ = delete_device(State(app.state.clone()), peer_headers("a"), Path("a".into())).await.unwrap();
        let inner = app.state.inner.read().await;
        assert!(inner.devices.contains_key("z"));
        assert_eq!(authenticate_device(&inner, "alice", "a", "a-token").err().unwrap().status, StatusCode::GONE);
    }

    #[tokio::test]
    async fn device_delete_rejects_running_and_cleaning_diagnostics() {
        let app = test_app();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        app.state.configure_diagnostics(DiagnosticIntegration {
            admin_url: format!("http://{address}"), admin_user: "test".into(),
            admin_password: "test".into(), plugin_token: "test".into(),
        }).await;
        let admin = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/api/serverinfo", get(|| async { Json(serde_json::json!({})) }))).await.unwrap();
        });
        let info = diagnostics::create(State(app.state.clone()), peer_headers("a"), Json(starbridge_protocol::DiagnosticCreateRequest {
            node_id: "a".into(), device_token: "a-token".into(),
        })).await.unwrap().0;
        assert_eq!(delete_device(State(app.state.clone()), peer_headers("b"), Path("a".into())).await.unwrap_err().status, StatusCode::CONFLICT);
        app.state.inner.write().await.diagnostics.get_mut(&info.test_id).unwrap().info.status = "cleaning".into();
        assert_eq!(delete_device(State(app.state.clone()), peer_headers("b"), Path("a".into())).await.unwrap_err().status, StatusCode::CONFLICT);
        app.state.inner.write().await.diagnostics.get_mut(&info.test_id).unwrap().info.status = "cancelled".into();
        let _ = delete_device(State(app.state.clone()), peer_headers("b"), Path("a".into())).await.unwrap();
        admin.abort();
    }

    #[tokio::test]
    async fn metadata_survives_reregistration_and_does_not_rebuild_connections() {
        let app = test_app();
        let session = create_peer_session(&app.state, "a", "b").await;
        let original = app.state.inner.read().await.devices["b"].registration.clone();
        let _ = update_device_metadata(State(app.state.clone()), peer_headers("a"), Path("b".into()), Json(DeviceMetadataUpdate {
            display_name: Some("Build server".into()), purpose: Some("Build releases".into()),
        })).await.unwrap();
        let _ = register_device(State(app.state.clone()), peer_headers("b"), Json(original)).await.unwrap();
        let inner = app.state.inner.read().await;
        assert_eq!(inner.devices["b"].registration.display_name, "Build server");
        assert_eq!(inner.devices["b"].registration.purpose, "Build releases");
        assert!(is_active(&inner.sessions[&session.session_id]));
    }

    #[tokio::test]
    async fn metadata_and_registration_accept_chinese_character_boundaries() {
        let app = test_app();
        let name = "设".repeat(80);
        let purpose = "用".repeat(240);
        let mut registration = app.state.inner.read().await.devices["b"].registration.clone();
        registration.display_name = name.clone();
        registration.purpose = purpose.clone();
        assert!(validate_registration(&registration).is_ok());
        let _ = update_device_metadata(State(app.state.clone()), peer_headers("a"), Path("b".into()), Json(DeviceMetadataUpdate {
            display_name: Some(name.clone()), purpose: Some(purpose.clone()),
        })).await.unwrap();
        {
            let inner = app.state.inner.read().await;
            assert_eq!(inner.devices["b"].registration.display_name, name);
            assert_eq!(inner.devices["b"].registration.purpose, purpose);
        }
        for (name, purpose) in [("设".repeat(81), "用".repeat(240)), ("设".repeat(80), "用".repeat(241))] {
            registration.display_name = name.clone();
            registration.purpose = purpose.clone();
            assert_eq!(validate_registration(&registration).unwrap_err().status, StatusCode::BAD_REQUEST);
            assert_eq!(update_device_metadata(State(app.state.clone()), peer_headers("a"), Path("b".into()), Json(DeviceMetadataUpdate {
                display_name: Some(name), purpose: Some(purpose),
            })).await.unwrap_err().status, StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn registration_login_refresh_rotation_restart_and_logout() {
        let app = test_app();
        let credentials = || AuthRequest { username: "new-user".into(), password: "test-password".into() };
        assert_eq!(login(State(app.state.clone()), Json(credentials())).await.unwrap_err().status, StatusCode::UNAUTHORIZED);
        let auth = register_account(State(app.state.clone()), Json(credentials())).await.unwrap().0;
        assert_eq!(register_account(State(app.state.clone()), Json(credentials())).await.unwrap_err().status, StatusCode::CONFLICT);
        assert!(login(State(app.state.clone()), Json(credentials())).await.is_ok());
        let old_refresh = auth.refresh_token.unwrap();
        let restored = AppState::with_frp_and_data("localhost".into(), 17851, "token".into(), app.state.data_path.as_ref().clone());
        let refreshed = refresh_auth(State(restored.clone()), Json(AuthRefreshRequest { refresh_token: old_refresh.clone() })).await.unwrap().0;
        assert!(refresh_auth(State(restored.clone()), Json(AuthRefreshRequest { refresh_token: old_refresh })).await.is_err());
        let new_refresh = refreshed.refresh_token.clone().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, format!("Bearer {}", refreshed.access_token).parse().unwrap());
        assert_eq!(authenticate(&restored, &headers).await.unwrap(), "new-user");
        let _ = logout_auth(State(restored.clone()), headers.clone(), Json(AuthLogoutRequest { refresh_token: Some(new_refresh.clone()) })).await.unwrap();
        assert!(authenticate(&restored, &headers).await.is_err());
        assert!(refresh_auth(State(restored), Json(AuthRefreshRequest { refresh_token: new_refresh })).await.is_err());
        let persisted = std::fs::read_to_string(app.state.data_path.join("server-state.json")).unwrap();
        assert!(!persisted.contains("test-password"));
        assert!(!persisted.contains(&refreshed.refresh_token.unwrap()));
    }

    #[tokio::test]
    async fn refresh_expiration_and_racing_rotation_are_enforced() {
        let app = test_app();
        let auth = login(State(app.state.clone()), Json(AuthRequest { username: "alice".into(), password: "test1234".into() })).await.unwrap().0;
        let token = auth.refresh_token.unwrap();
        let (a, b) = tokio::join!(
            refresh_auth(State(app.state.clone()), Json(AuthRefreshRequest { refresh_token: token.clone() })),
            refresh_auth(State(app.state.clone()), Json(AuthRefreshRequest { refresh_token: token.clone() })),
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        let auth = a.ok().or(b.ok()).unwrap().0;
        let token = auth.refresh_token.unwrap();
        app.state.inner.write().await.refresh_grants.get_mut(&password_digest(&token)).unwrap().expires_at = now_unix() - 1;
        assert!(refresh_auth(State(app.state.clone()), Json(AuthRefreshRequest { refresh_token: token })).await.is_err());
    }

    fn identified_registration(id: &str, token: &str, key_digit: &str) -> DeviceRegistration {
        let mut registration = device(false, true);
        registration.node_id = id.into();
        registration.device_token = token.into();
        registration.machine_key = Some(key_digit.repeat(64));
        registration.capabilities = vec!["ssh_client".into(), "ssh_server".into(), "ssh".into()];
        registration.ssh_public_key = format!("ssh-ed25519 {id}-public");
        registration.ssh_host_key = format!("ssh-ed25519 {id}-host");
        registration
    }

    fn rebind_request(registration: DeviceRegistration) -> DeviceRebindRequest {
        DeviceRebindRequest {
            registration, auth: AuthRequest { username: "alice".into(), password: "test1234".into() },
        }
    }

    #[tokio::test]
    async fn machine_identity_reuses_node_across_login_restart_and_client_directory() {
        let app = test_app();
        let registration = identified_registration("a", "a-token", "a");
        let initial_count = app.state.inner.read().await.devices.len();
        let first = identify_device(State(app.state.clone()), peer_headers("a"), Json(registration.clone())).await.unwrap().0;
        assert_eq!(first.node_id, "a");
        let _ = login(State(app.state.clone()), Json(AuthRequest { username: "alice".into(), password: "test1234".into() })).await.unwrap();
        let repeated = identify_device(State(app.state.clone()), peer_headers("a"), Json(registration.clone())).await.unwrap().0;
        assert_eq!(repeated.node_id, "a");
        let mut other_directory = registration.clone();
        other_directory.node_id = "different-extraction".into();
        let adopted = identify_device(State(app.state.clone()), peer_headers("a"), Json(other_directory.clone())).await.unwrap().0;
        assert_eq!(adopted.node_id, "a");
        assert_eq!(app.state.inner.read().await.devices.len(), initial_count);
        let restored = AppState::with_frp_and_data("localhost".into(), 17851, "token".into(), app.state.data_path.as_ref().clone());
        let auth = login(State(restored.clone()), Json(AuthRequest { username: "alice".into(), password: "test1234".into() })).await.unwrap().0;
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, format!("Bearer {}", auth.access_token).parse().unwrap());
        assert_eq!(identify_device(State(restored.clone()), headers, Json(other_directory)).await.unwrap().0.node_id, "a");
        assert_eq!(restored.inner.read().await.devices.len(), initial_count);
        let listing = serde_json::to_string(&adopted.devices).unwrap();
        assert!(!listing.contains("machine_key") && !listing.contains("device_token"));
    }

    #[tokio::test]
    async fn machine_keys_are_scoped_to_accounts_and_distinct_machines_remain_separate() {
        let app = test_app();
        let a = identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("a", "a-token", "a"))).await.unwrap().0;
        let other = identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("new-machine", "other-token", "b"))).await.unwrap().0;
        assert_ne!(a.node_id, other.node_id);
        let mut bob = HeaderMap::new();
        bob.insert(header::AUTHORIZATION, "Bearer bob-access".parse().unwrap());
        let separate_account = identify_device(State(app.state.clone()), bob, Json(identified_registration("a", "bob-new-token", "a"))).await.unwrap().0;
        assert_ne!(separate_account.node_id, a.node_id);
        assert!(separate_account.devices.iter().all(|device| device.account == "bob"));
        assert_eq!(app.state.inner.read().await.devices["a"].device_token, "a-token");
    }

    #[tokio::test]
    async fn matching_machine_is_not_authentication_and_rebind_revokes_old_credential() {
        let app = test_app();
        let original = identified_registration("a", "a-token", "a");
        let _ = identify_device(State(app.state.clone()), peer_headers("a"), Json(original.clone())).await.unwrap();
        let mut replacement = identified_registration("fresh-install", "new-device-token", "a");
        replacement.display_name = "must not replace existing name".into();
        assert_eq!(identify_device(State(app.state.clone()), peer_headers("a"), Json(replacement.clone())).await.unwrap_err().status, StatusCode::CONFLICT);
        let mut wrong_auth = rebind_request(replacement.clone());
        wrong_auth.auth.password = "wrong-password".into();
        assert_eq!(rebind_device(State(app.state.clone()), peer_headers("a"), Json(wrong_auth)).await.unwrap_err().status, StatusCode::UNAUTHORIZED);
        let response = rebind_device(State(app.state.clone()), peer_headers("a"), Json(rebind_request(replacement.clone()))).await.unwrap().0;
        assert_eq!(response.node_id, "a");
        assert_eq!(app.state.inner.read().await.devices["a"].registration.display_name, original.display_name);
        assert_eq!(heartbeat(State(app.state.clone()), peer_headers("a"), Json(HeartbeatRequest {
            node_id: "a".into(), device_token: "a-token".into(), status: "online".into(),
        })).await.unwrap_err().status, StatusCode::GONE);
        assert_eq!(identify_device(State(app.state.clone()), peer_headers("a"), Json(original.clone())).await.unwrap_err().status, StatusCode::GONE);
        assert_eq!(rebind_device(State(app.state.clone()), peer_headers("a"), Json(rebind_request(original))).await.unwrap_err().status, StatusCode::GONE);
        let restored = AppState::with_frp_and_data("localhost".into(), 17851, "token".into(), app.state.data_path.as_ref().clone());
        let inner = restored.inner.read().await;
        assert_eq!(authenticate_device(&inner, "alice", "a", "a-token").err().unwrap().status, StatusCode::GONE);
        assert!(authenticate_device(&inner, "alice", "a", "new-device-token").is_ok());
    }

    #[tokio::test]
    async fn deleted_machine_cannot_be_restored_by_background_registration_or_a_new_id() {
        let app = test_app();
        let original = identified_registration("b", "b-token", "b");
        let _ = identify_device(State(app.state.clone()), peer_headers("b"), Json(original.clone())).await.unwrap();
        let _ = delete_device(State(app.state.clone()), peer_headers("a"), Path("b".into())).await.unwrap();
        let replacement = identified_registration("new-local-id", "new-local-token", "b");
        for registration in [original.clone(), replacement.clone()] {
            assert_eq!(identify_device(State(app.state.clone()), peer_headers("b"), Json(registration)).await.unwrap_err().status, StatusCode::GONE);
        }
        let restored = AppState::with_frp_and_data("localhost".into(), 17851, "token".into(), app.state.data_path.as_ref().clone());
        assert_eq!(restored.inner.read().await.deleted_devices["b"].machine_key, original.machine_key);
        let response = rebind_device(State(app.state.clone()), peer_headers("a"), Json(rebind_request(replacement))).await.unwrap().0;
        assert_eq!(response.node_id, "b");
        assert_eq!(identify_device(State(app.state.clone()), peer_headers("b"), Json(original)).await.unwrap_err().status, StatusCode::GONE);
    }

    #[tokio::test]
    async fn legacy_attachment_requires_original_credential_and_machine_source_cannot_change() {
        let app = test_app();
        let mut registration = identified_registration("a", "wrong-token", "a");
        assert_eq!(identify_device(State(app.state.clone()), peer_headers("a"), Json(registration.clone())).await.unwrap_err().status, StatusCode::CONFLICT);
        assert_eq!(rebind_device(State(app.state.clone()), peer_headers("a"), Json(rebind_request(registration.clone()))).await.unwrap_err().status, StatusCode::CONFLICT);
        registration.device_token = "a-token".into();
        let _ = identify_device(State(app.state.clone()), peer_headers("a"), Json(registration.clone())).await.unwrap();
        registration.machine_key = None;
        assert_eq!(register_device(State(app.state.clone()), peer_headers("a"), Json(registration.clone())).await.unwrap_err().status, StatusCode::CONFLICT);
        registration.machine_key = Some("b".repeat(64));
        assert_eq!(identify_device(State(app.state.clone()), peer_headers("a"), Json(registration.clone())).await.unwrap_err().status, StatusCode::CONFLICT);
        assert_eq!(rebind_device(State(app.state.clone()), peer_headers("a"), Json(rebind_request(registration))).await.unwrap_err().status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn duplicate_legacy_records_are_reported_without_merging_or_deleting() {
        let app = test_app();
        let _ = identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("a", "a-token", "a"))).await.unwrap();
        let duplicate = identified_registration("b", "b-token", "a");
        assert_eq!(identify_device(State(app.state.clone()), peer_headers("b"), Json(duplicate.clone())).await.unwrap_err().status, StatusCode::CONFLICT);
        {
            let mut inner = app.state.inner.write().await;
            assert!(inner.devices["b"].registration.machine_key.is_none());
            inner.devices.get_mut("b").unwrap().registration.machine_key = duplicate.machine_key.clone();
        }
        assert_eq!(identify_device(State(app.state.clone()), peer_headers("b"), Json(duplicate)).await.unwrap_err().status, StatusCode::CONFLICT);
        let inner = app.state.inner.read().await;
        assert!(inner.devices.contains_key("a") && inner.devices.contains_key("b"));
        drop(inner);
        let _ = delete_device(State(app.state.clone()), peer_headers("c"), Path("b".into())).await.unwrap();
        let retained = identified_registration("a", "a-token", "a");
        assert_eq!(identify_device(State(app.state.clone()), peer_headers("a"), Json(retained.clone())).await.unwrap().0.node_id, "a");
        assert_eq!(identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("new-id", "b-token", "a"))).await.unwrap_err().status, StatusCode::GONE);
        let _ = delete_device(State(app.state.clone()), peer_headers("c"), Path("a".into())).await.unwrap();
        let new_identity = identified_registration("explicit-new-record", "new-safe-token", "a");
        assert_eq!(identify_device(State(app.state.clone()), peer_headers("c"), Json(new_identity.clone())).await.unwrap_err().status, StatusCode::CONFLICT);
        assert_eq!(rebind_device(State(app.state.clone()), peer_headers("c"), Json(rebind_request(new_identity.clone()))).await.unwrap().0.node_id, "explicit-new-record");
        assert_eq!(identify_device(State(app.state.clone()), peer_headers("c"), Json(new_identity)).await.unwrap().0.node_id, "explicit-new-record");
    }

    #[tokio::test]
    async fn concurrent_machine_registrations_do_not_overwrite_credentials() {
        let app = test_app();
        let (first, second) = tokio::join!(
            identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("race-a", "race-a-token", "c"))),
            identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("race-b", "race-b-token", "c"))),
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert_eq!(first.err().or(second.err()).unwrap().status, StatusCode::CONFLICT);
        let inner = app.state.inner.read().await;
        let matches: Vec<_> = inner.devices.values().filter(|device| device.registration.machine_key == Some("c".repeat(64))).collect();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].device_token == "race-a-token" || matches[0].device_token == "race-b-token");
    }

    #[tokio::test]
    async fn explicit_isolated_instances_and_same_credential_races_keep_their_own_nodes() {
        let app = test_app();
        let (first, second) = tokio::join!(
            identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("same-a", "same-token", "c"))),
            identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("same-b", "same-token", "c"))),
        );
        assert_eq!(first.unwrap().0.node_id, second.unwrap().0.node_id);
        let test_one = identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("test-one", "test-one-token", "d"))).await.unwrap().0;
        let test_two = identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("test-two", "test-two-token", "e"))).await.unwrap().0;
        assert_ne!(test_one.node_id, test_two.node_id);
    }

    #[tokio::test]
    async fn rebind_response_retry_is_idempotent_and_busy_nodes_keep_their_original_credential() {
        let app = test_app();
        let _ = identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("a", "a-token", "a"))).await.unwrap();
        let session = create_peer_session(&app.state, "a", "b").await;
        let replacement = identified_registration("new-install", "replacement-token", "a");
        assert_eq!(rebind_device(State(app.state.clone()), peer_headers("a"), Json(rebind_request(replacement.clone()))).await.unwrap_err().status, StatusCode::CONFLICT);
        assert_eq!(app.state.inner.read().await.devices["a"].device_token, "a-token");
        let _ = delete_session(State(app.state.clone()), peer_headers("a"), Path(session.session_id)).await.unwrap();
        let _lost_response = rebind_device(State(app.state.clone()), peer_headers("a"), Json(rebind_request(replacement.clone()))).await.unwrap();
        assert_eq!(identify_device(State(app.state.clone()), peer_headers("a"), Json(replacement.clone())).await.unwrap().0.node_id, "a");
        assert_eq!(rebind_device(State(app.state.clone()), peer_headers("a"), Json(rebind_request(replacement))).await.unwrap().0.node_id, "a");
        let inner = app.state.inner.read().await;
        assert!(!inner.deleted_devices["a"].revoked_token_hashes.contains(&password_digest("replacement-token")));
        assert_eq!(inner.deleted_devices["a"].revoked_token_hashes.len(), 1);
    }

    #[tokio::test]
    async fn failed_persistence_rolls_back_rebind_and_revocation() {
        let app = test_app();
        let _ = identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("a", "a-token", "a"))).await.unwrap();
        let blocked_path = app.state.data_path.join("not-a-directory");
        std::fs::write(&blocked_path, b"block persistence").unwrap();
        let failing = AppState { data_path: Arc::new(blocked_path), ..app.state.clone() };
        let error = rebind_device(State(failing), peer_headers("a"), Json(rebind_request(identified_registration("new-install", "replacement-token", "a")))).await.unwrap_err();
        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        let inner = app.state.inner.read().await;
        assert!(authenticate_device(&inner, "alice", "a", "a-token").is_ok());
        assert_eq!(inner.devices["a"].device_token, "a-token");
        assert!(!inner.deleted_devices.contains_key("a"));
        let disk = load_persisted(&app.state.data_path).unwrap();
        assert_eq!(disk.devices["a"].device_token, "a-token");
        assert!(!disk.deleted_devices.contains_key("a"));
    }

    #[tokio::test]
    async fn revoked_credentials_stay_revoked_through_delete_rebind_and_restart() {
        let app = test_app();
        let _ = identify_device(State(app.state.clone()), peer_headers("a"), Json(identified_registration("a", "a-token", "a"))).await.unwrap();
        let _ = rebind_device(State(app.state.clone()), peer_headers("a"), Json(rebind_request(identified_registration("new-install", "second-token", "a")))).await.unwrap();
        let _ = delete_device(State(app.state.clone()), peer_headers("b"), Path("a".into())).await.unwrap();
        let _ = rebind_device(State(app.state.clone()), peer_headers("b"), Json(rebind_request(identified_registration("third-install", "third-token", "a")))).await.unwrap();
        let restored = AppState::with_frp_and_data("localhost".into(), 17851, "token".into(), app.state.data_path.as_ref().clone());
        let inner = restored.inner.read().await;
        assert!(authenticate_device(&inner, "alice", "a", "third-token").is_ok());
        for revoked in ["a-token", "second-token"] {
            assert_eq!(authenticate_device(&inner, "alice", "a", revoked).err().unwrap().status, StatusCode::GONE);
        }
    }

    #[test]
    fn machine_key_wire_format_is_strict_and_not_copied_into_listings() {
        let mut registration = identified_registration("test", "test-token", "a");
        assert!(validate_registration(&registration).is_ok());
        for invalid in ["", "a", &"A".repeat(64), &"g".repeat(64), &"0".repeat(65)] {
            registration.machine_key = Some(invalid.into());
            assert_eq!(validate_registration(&registration).unwrap_err().status, StatusCode::BAD_REQUEST);
        }
        registration.machine_key = None;
        assert!(validate_registration(&registration).is_ok());
    }
}
