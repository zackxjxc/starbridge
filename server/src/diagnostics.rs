//! FRP's official HTTP plugin gates diagnostic access; its dashboard supplies
//! independent, per-route traffic evidence. Neither a client status nor a
//! successful proxy registration alone establishes that relay traffic passed.
use super::*;
use axum::extract::Query;
use starbridge_protocol::{DiagnosticCreateRequest, DiagnosticSession, DiagnosticUpdateRequest, new_token};

#[derive(Clone)]
pub struct DiagnosticIntegration {
    pub admin_url: String,
    pub admin_user: String,
    pub admin_password: String,
    pub plugin_token: String,
}

#[derive(Clone)]
pub(super) struct DiagnosticRecord {
    pub info: DiagnosticSession,
    account: String,
    client_cleaned: bool,
    proxy_registered: bool,
    proxy_closed: bool,
    user_connection_seen: bool,
    outcome: Option<String>,
}

impl AppState {
    pub async fn configure_diagnostics(&self, integration: DiagnosticIntegration) {
        *self.diagnostics.write().await = Some(integration);
    }

    /// Independently expires diagnostic access even when the client disappears.
    pub fn start_diagnostic_maintenance(&self) -> tokio::task::JoinHandle<()> {
        let state = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                let ids: Vec<_> = {
                    let mut inner = state.inner.write().await;
                    expire(&mut inner, now_unix());
                    inner.diagnostics.values().filter(|record| record.info.status == "cleaning")
                        .map(|record| record.info.test_id.clone()).collect()
                };
                for id in ids { observe(&state, &id).await; }
            }
        })
    }
}

pub(super) fn occupied(inner: &Inner, node: &str) -> bool {
    inner.diagnostics.values().any(|record| record.info.node_id == node
        && matches!(record.info.status.as_str(), "running" | "cleaning"))
}

pub(super) fn expire(inner: &mut Inner, now: u64) {
    for record in inner.diagnostics.values_mut() {
        if record.info.status == "running" && (record.info.expires_at <= now
            || !inner.devices.get(&record.info.node_id).is_some_and(|device| device.status == "online"
                && now.saturating_sub(device.last_heartbeat) <= 30)) {
            record.info.status = "cleaning".into();
            record.info.phase = "cleanup".into();
            record.outcome = Some("expired".into());
            record.info.failure_reason = Some("连接测试已过期或设备已离线".into());
        }
    }
}

fn unsupported() -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "服务器尚不支持完整连接测试，尚未验证；需要启用受管 FRPS 诊断集成")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().timeout(std::time::Duration::from_secs(5)).build().expect("HTTP client")
}

fn local_address(address: &str) -> bool {
    address.eq_ignore_ascii_case("localhost") || address.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

pub(super) async fn create(
    State(state): State<AppState>, headers: HeaderMap, Json(request): Json<DiagnosticCreateRequest>,
) -> Result<Json<DiagnosticSession>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    let integration = state.diagnostics.read().await.clone().ok_or_else(unsupported)?;
    // Fail closed when the configured FRPS is absent or its protected API fails.
    client().get(format!("{}/api/serverinfo", integration.admin_url))
        .basic_auth(&integration.admin_user, Some(&integration.admin_password)).send().await
        .and_then(|response| response.error_for_status()).map_err(|_| unsupported())?;
    let mut inner = state.inner.write().await;
    expire_sessions(&mut inner, now_unix());
    expire(&mut inner, now_unix());
    let device = authenticate_device(&inner, &account, &request.node_id, &request.device_token)?;
    if !is_online(device) { return Err(ApiError::conflict("本机当前离线，不能进行连接测试")); }
    if device.registration.direct_only || !device.registration.allow_relay {
        return Err(ApiError::conflict("当前策略不允许中转测试，尚未验证"));
    }
    if occupied(&inner, &request.node_id) || inner.sessions.values().any(|session| is_active(session)
        && (session.initiator_node_id == request.node_id || session.target_node_id == request.node_id)) {
        return Err(ApiError::conflict("本机正在连接或测试，请先终止相关任务"));
    }
    let id = new_id();
    let now = now_unix();
    let info = DiagnosticSession {
        test_id: id.clone(), node_id: request.node_id, status: "running".into(), phase: "prepare".into(),
        created_at: now, expires_at: now + 300, challenge: new_token(),
        route: FrpRoute { server_addr: inner.frp_server_addr.clone(), server_port: inner.frp_server_port,
            auth_token: inner.frp_auth_token.clone(), proxy_name: format!("sb-test-{id}"), secret_key: new_token(), remote_port: 0 },
        relay_verified: false, ssh_verified: false, cleanup_complete: false,
        local_relay: local_address(&inner.frp_server_addr), failure_reason: None,
    };
    // Both ordinary and diagnostic reservations use Inner's same write lock.
    inner.diagnostics.insert(id, DiagnosticRecord { info: info.clone(), account, client_cleaned: false,
        proxy_registered: false, proxy_closed: false, user_connection_seen: false, outcome: None });
    Ok(Json(info))
}

async fn authorize(state: &AppState, headers: &HeaderMap, id: &str) -> Result<(), ApiError> {
    let account = authenticate(state, headers).await?;
    let inner = state.inner.read().await;
    let record = inner.diagnostics.get(id).ok_or_else(|| ApiError::not_found("连接测试不存在或服务已重启"))?;
    let node = headers.get("x-starbridge-node-id").and_then(|value| value.to_str().ok()).unwrap_or("");
    authenticate_device(&inner, &account, node, device_token_header(headers)?)?;
    if account != record.account || node != record.info.node_id { return Err(ApiError::forbidden("只能操作本机发起的连接测试")); }
    Ok(())
}

async fn current_id(state: &AppState, headers: &HeaderMap, node_id: &str) -> Result<Option<String>, ApiError> {
    let account = authenticate(state, headers).await?;
    let header_node = headers.get("x-starbridge-node-id").and_then(|value| value.to_str().ok()).unwrap_or("");
    if header_node != node_id { return Err(ApiError::forbidden("只能查询本机的连接测试")); }
    let inner = state.inner.read().await;
    authenticate_device(&inner, &account, node_id, device_token_header(headers)?)?;
    Ok(inner.diagnostics.values().find(|record| record.info.node_id == node_id && record.account == account
        && matches!(record.info.status.as_str(), "running" | "cleaning")).map(|record| record.info.test_id.clone()))
}

pub(super) async fn current(State(state): State<AppState>, headers: HeaderMap, Path(node_id): Path<String>) -> Result<Json<Option<DiagnosticSession>>, ApiError> {
    match current_id(&state, &headers, &node_id).await? {
        Some(id) => Ok(Json(Some(get(State(state), headers, Path(id)).await?.0))),
        None => Ok(Json(None)),
    }
}

pub(super) async fn close_current(State(state): State<AppState>, headers: HeaderMap, Path(node_id): Path<String>) -> Result<Json<Option<DiagnosticSession>>, ApiError> {
    match current_id(&state, &headers, &node_id).await? {
        Some(id) => Ok(Json(Some(close(State(state), headers, Path(id)).await?.0))),
        None => Ok(Json(None)),
    }
}

pub(super) async fn get(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>) -> Result<Json<DiagnosticSession>, ApiError> {
    authorize(&state, &headers, &id).await?;
    { expire(&mut *state.inner.write().await, now_unix()); }
    observe(&state, &id).await;
    Ok(Json(state.inner.read().await.diagnostics[&id].info.clone()))
}

pub(super) async fn update(
    State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, Json(request): Json<DiagnosticUpdateRequest>,
) -> Result<Json<DiagnosticSession>, ApiError> {
    authorize(&state, &headers, &id).await?;
    if request.phase.len() > 80 || request.failure_reason.as_ref().is_some_and(|reason| reason.len() > 2000)
        || request.outcome.as_deref().is_some_and(|outcome| !matches!(outcome, "succeeded" | "failed" | "cancelled")) {
        return Err(ApiError::bad_request("连接测试状态格式错误"));
    }
    {
        let mut inner = state.inner.write().await;
        expire(&mut inner, now_unix());
        let record = inner.diagnostics.get_mut(&id).expect("authorized record");
        if !matches!(record.info.status.as_str(), "running" | "cleaning") {
            return Ok(Json(record.info.clone()));
        }
        if record.info.status == "running" {
            if !request.phase.is_empty() { record.info.phase = request.phase; }
            record.info.ssh_verified |= request.ssh_verified;
            if request.failure_reason.is_some() { record.info.failure_reason = request.failure_reason; }
        }
        record.client_cleaned |= request.cleanup_complete;
        if let Some(outcome) = request.outcome {
            if record.outcome.is_none() { record.outcome = Some(outcome); }
            record.info.status = "cleaning".into();
            record.info.phase = "cleanup".into();
        }
    }
    observe(&state, &id).await;
    Ok(Json(state.inner.read().await.diagnostics[&id].info.clone()))
}

pub(super) async fn close(State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>) -> Result<Json<DiagnosticSession>, ApiError> {
    authorize(&state, &headers, &id).await?;
    {
        let mut inner = state.inner.write().await;
        let record = inner.diagnostics.get_mut(&id).expect("authorized record");
        if record.info.status == "running" {
            record.info.status = "cleaning".into();
            record.info.phase = "cleanup".into();
            record.outcome = Some("cancelled".into());
        }
    }
    observe(&state, &id).await;
    Ok(Json(state.inner.read().await.diagnostics[&id].info.clone()))
}

async fn observe(state: &AppState, id: &str) {
    let Some(integration) = state.diagnostics.read().await.clone() else { return; };
    let name = match state.inner.read().await.diagnostics.get(id) {
        Some(record) => record.info.route.proxy_name.clone(), None => return,
    };
    let response = client().get(format!("{}/api/proxy/stcp/{name}", integration.admin_url))
        .basic_auth(&integration.admin_user, Some(&integration.admin_password)).send().await;
    let (traffic, gone) = match response {
        Ok(response) if response.status() == StatusCode::NOT_FOUND => (false, true),
        Ok(response) if response.status().is_success() => {
            let Ok(value) = response.json::<serde_json::Value>().await else { return; };
            (value["todayTrafficIn"].as_u64().unwrap_or(0) > 0 && value["todayTrafficOut"].as_u64().unwrap_or(0) > 0,
                value["status"].as_str() == Some("offline") && value["curConns"].as_u64().unwrap_or(0) == 0)
        }
        _ => return, // Missing evidence is not success and not proof of cleanup.
    };
    let mut inner = state.inner.write().await;
    let Some(record) = inner.diagnostics.get_mut(id) else { return; };
    record.info.relay_verified |= traffic && record.user_connection_seen && !record.info.local_relay;
    if record.info.status != "cleaning" { return; }
    if !gone { return; }
    if record.client_cleaned {
        record.info.cleanup_complete = true;
        let outcome = record.outcome.as_deref().unwrap_or("failed");
        record.info.status = if outcome == "succeeded" && !(record.info.ssh_verified && record.info.relay_verified) {
            record.info.failure_reason = Some(if record.info.local_relay { "仅完成本地联调，未验证远程中转" } else { "未取得真实 SSH 与服务器中转的完整验证证据" }.into());
            "failed".into()
        } else { outcome.into() };
        record.info.phase = "finished".into();
    } else if record.outcome.as_deref() == Some("expired") {
        // The data-plane authorization is revoked; local crash residue still
        // requires the client's startup recovery and must not be called clean.
        record.info.status = "expired".into();
        record.info.failure_reason = Some("测试已过期，服务器访问已撤销；本机临时资源清理尚未确认".into());
    }
}

/// Bind this router only to loopback. The unguessable path is never sent to clients.
pub fn diagnostic_plugin_app(state: AppState, token: &str) -> Router {
    Router::new().route(&format!("/frp/{token}"), post(plugin)).with_state(state)
}

#[derive(Deserialize)]
struct PluginQuery { op: String }

async fn plugin(State(state): State<AppState>, Query(query): Query<PluginQuery>, Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
    let content = &body["content"];
    let metadata = if query.op == "Login" { &content["metas"] } else { &content["user"]["metas"] };
    let id = metadata["starbridge_test_id"].as_str().unwrap_or("");
    let name = content["proxy_name"].as_str().unwrap_or("");
    let allow = || Json(serde_json::json!({"reject": false, "unchange": true}));
    let reject = || Json(serde_json::json!({"reject": true, "reject_reason": "diagnostic authorization invalid or expired"}));
    if id.is_empty() { return if name.starts_with("sb-test-") { reject() } else { allow() }; }
    let mut inner = state.inner.write().await;
    expire(&mut inner, now_unix());
    let Some(record) = inner.diagnostics.get_mut(id) else { return reject(); };
    if metadata["starbridge_test_token"].as_str() != Some(record.info.route.secret_key.as_str()) { return reject(); }
    if query.op == "CloseProxy" {
        if name == record.info.route.proxy_name { record.proxy_closed = true; record.proxy_registered = false; }
        return allow();
    }
    if record.info.status != "running" || record.info.expires_at <= now_unix() { return reject(); }
    match query.op.as_str() {
        "NewProxy" => {
            if name != record.info.route.proxy_name || content["proxy_type"].as_str() != Some("stcp")
                || content["sk"].as_str() != Some(record.info.route.secret_key.as_str()) { return reject(); }
            record.proxy_registered = true;
            record.proxy_closed = false;
        }
        "NewUserConn" => {
            if name != record.info.route.proxy_name || content["proxy_type"].as_str() != Some("stcp") { return reject(); }
            record.user_connection_seen = true;
        }
        "Login" | "Ping" | "NewWorkConn" => {}
        _ => return reject(),
    }
    allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{peer_headers, session_request, test_app};

    async fn admin(state: &AppState) -> (Arc<RwLock<serde_json::Value>>, tokio::task::JoinHandle<()>) {
        let stats = Arc::new(RwLock::new(serde_json::json!({"status": "online", "curConns": 0, "todayTrafficIn": 0, "todayTrafficOut": 0})));
        let mock = stats.clone();
        let router = Router::new().route("/api/serverinfo", axum::routing::get(|| async { Json(serde_json::json!({"version": "0.71.1"})) }))
            .route("/api/proxy/stcp/{name}", axum::routing::get(move || { let stats = mock.clone(); async move { Json(stats.read().await.clone()) } }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        state.configure_diagnostics(DiagnosticIntegration { admin_url: format!("http://{address}"), admin_user: "test".into(),
            admin_password: "test".into(), plugin_token: "test".into() }).await;
        let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        (stats, handle)
    }

    async fn start(state: &AppState) -> DiagnosticSession {
        create(State(state.clone()), peer_headers("a"), Json(DiagnosticCreateRequest {
            node_id: "a".into(), device_token: "a-token".into(),
        })).await.unwrap().0
    }

    fn event(info: &DiagnosticSession, operation: &str) -> (Query<PluginQuery>, Json<serde_json::Value>) {
        (Query(PluginQuery { op: operation.into() }), Json(serde_json::json!({"content": {
            "user": {"metas": {"starbridge_test_id": info.test_id, "starbridge_test_token": info.route.secret_key}},
            "proxy_name": info.route.proxy_name, "proxy_type": "stcp", "sk": info.route.secret_key,
        }})))
    }

    #[tokio::test]
    async fn diagnostics_require_integration_policy_and_exclusive_device() {
        let app = test_app();
        assert_eq!(create(State(app.state.clone()), peer_headers("a"), Json(DiagnosticCreateRequest {
            node_id: "a".into(), device_token: "a-token".into(),
        })).await.unwrap_err().status, StatusCode::SERVICE_UNAVAILABLE);
        let (_, admin) = admin(&app.state).await;
        let info = start(&app.state).await;
        assert!(info.local_relay);
        assert_eq!(occupancy(&*app.state.inner.read().await, "a").0, "diagnostic");
        for (from, to) in [("a", "b"), ("b", "a")] {
            assert_eq!(create_session(State(app.state.clone()), peer_headers(from), Json(session_request(from, to)))
                .await.unwrap_err().status, StatusCode::CONFLICT);
        }
        assert!(get(State(app.state.clone()), peer_headers("b"), Path(info.test_id)).await.is_err());
        assert_eq!(create(State(app.state.clone()), peer_headers("a"), Json(DiagnosticCreateRequest {
            node_id: "a".into(), device_token: "a-token".into(),
        })).await.unwrap_err().status, StatusCode::CONFLICT);
        app.state.inner.write().await.devices.get_mut("b").unwrap().registration.direct_only = true;
        assert!(create(State(app.state.clone()), peer_headers("b"), Json(DiagnosticCreateRequest {
            node_id: "b".into(), device_token: "b-token".into(),
        })).await.unwrap_err().message.contains("不允许中转"));
        admin.abort();
    }

    #[tokio::test]
    async fn passing_requires_server_traffic_real_ssh_and_confirmed_cleanup() {
        let app = test_app();
        app.state.inner.write().await.frp_server_addr = "relay.example.test".into();
        let (stats, admin) = admin(&app.state).await;
        let info = start(&app.state).await;
        let (query, body) = event(&info, "NewProxy");
        assert_eq!(plugin(State(app.state.clone()), query, body).await.0["reject"], false);
        let partial = update(State(app.state.clone()), peer_headers("a"), Path(info.test_id.clone()), Json(DiagnosticUpdateRequest {
            ssh_verified: true, ..Default::default()
        })).await.unwrap().0;
        assert!(!partial.relay_verified);
        let (query, body) = event(&info, "NewUserConn");
        let _ = plugin(State(app.state.clone()), query, body).await;
        *stats.write().await = serde_json::json!({"status": "online", "curConns": 0, "todayTrafficIn": 123, "todayTrafficOut": 456});
        let evidence = get(State(app.state.clone()), peer_headers("a"), Path(info.test_id.clone())).await.unwrap().0;
        assert!(evidence.relay_verified);
        let cleaning = update(State(app.state.clone()), peer_headers("a"), Path(info.test_id.clone()), Json(DiagnosticUpdateRequest {
            cleanup_complete: true, outcome: Some("succeeded".into()), ..Default::default()
        })).await.unwrap().0;
        assert_eq!(cleaning.status, "cleaning");
        assert!(!cleaning.cleanup_complete);
        let (query, body) = event(&info, "NewUserConn");
        assert_eq!(plugin(State(app.state.clone()), query, body).await.0["reject"], true);
        stats.write().await["status"] = "offline".into();
        let result = get(State(app.state.clone()), peer_headers("a"), Path(info.test_id)).await.unwrap().0;
        assert_eq!(result.status, "succeeded");
        assert!(result.cleanup_complete);
        assert_eq!(occupancy(&*app.state.inner.read().await, "a").0, "idle");
        admin.abort();
    }

    #[tokio::test]
    async fn expired_access_is_rejected_and_local_relay_never_claims_remote_success() {
        let app = test_app();
        let (stats, admin) = admin(&app.state).await;
        let info = start(&app.state).await;
        let (query, body) = event(&info, "NewUserConn");
        let _ = plugin(State(app.state.clone()), query, body).await;
        *stats.write().await = serde_json::json!({"status": "offline", "curConns": 0, "todayTrafficIn": 123, "todayTrafficOut": 456});
        let result = update(State(app.state.clone()), peer_headers("a"), Path(info.test_id.clone()), Json(DiagnosticUpdateRequest {
            ssh_verified: true, cleanup_complete: true, outcome: Some("succeeded".into()), ..Default::default()
        })).await.unwrap().0;
        assert_eq!(result.status, "failed");
        assert!(!result.relay_verified);
        let next = start(&app.state).await;
        app.state.inner.write().await.diagnostics.get_mut(&next.test_id).unwrap().info.expires_at = now_unix() - 1;
        for operation in ["NewProxy", "NewUserConn", "Ping", "NewWorkConn"] {
            let (query, body) = event(&next, operation);
            assert_eq!(plugin(State(app.state.clone()), query, body).await.0["reject"], true);
        }
        let expired = get(State(app.state.clone()), peer_headers("a"), Path(next.test_id.clone())).await.unwrap().0;
        assert_eq!(expired.status, "expired");
        assert!(!expired.cleanup_complete);
        let late = update(State(app.state.clone()), peer_headers("a"), Path(next.test_id), Json(DiagnosticUpdateRequest {
            ssh_verified: true, cleanup_complete: true, outcome: Some("succeeded".into()), ..Default::default()
        })).await.unwrap().0;
        assert_eq!(late.status, "expired");
        admin.abort();
    }

    #[tokio::test]
    async fn cancel_revokes_access_and_cannot_be_changed_to_success() {
        let app = test_app();
        let (stats, admin) = admin(&app.state).await;
        assert!(current(State(app.state.clone()), peer_headers("a"), Path("a".into())).await.unwrap().0.is_none());
        let info = start(&app.state).await;
        assert!(close_current(State(app.state.clone()), peer_headers("b"), Path("a".into())).await.is_err());
        let recovered = close_current(State(app.state.clone()), peer_headers("a"), Path("a".into())).await.unwrap().0.unwrap();
        assert_eq!(recovered.test_id, info.test_id);
        let (query, body) = event(&info, "NewProxy");
        assert_eq!(plugin(State(app.state.clone()), query, body).await.0["reject"], true);
        stats.write().await["status"] = "offline".into();
        let result = update(State(app.state.clone()), peer_headers("a"), Path(info.test_id), Json(DiagnosticUpdateRequest {
            cleanup_complete: true, outcome: Some("succeeded".into()), ..Default::default()
        })).await.unwrap().0;
        assert_eq!(result.status, "cancelled");
        admin.abort();
    }
}
