//! Check the current session's receiving proxy before an XTCP visitor triggers
//! its first connection. A published SSH host key alone does not prove readiness.
use super::*;
use starbridge_protocol::SessionReadiness;

#[derive(Default, Deserialize)]
pub(super) struct ReadyQuery {
    ready_path: Option<SessionPath>,
}

fn expected_proxy(session: &SessionInfo, expected: SessionPath) -> Result<&str, ApiError> {
    if !matches!(expected, SessionPath::Direct | SessionPath::Relay) {
        return Err(ApiError::bad_request("通道类型必须为直连或中转"));
    }
    if !is_active(session) { return Err(ApiError::conflict("会话已结束，不能继续建立连接")); }
    let relay = session.policy == ConnectionPolicy::RelayOnly || session.path == SessionPath::Relay;
    if relay != (expected == SessionPath::Relay) {
        return Err(ApiError::conflict("连接方式已变化，请重新读取会话"));
    }
    session.route.as_ref().map(|route| route.proxy_name.as_str())
        .ok_or_else(|| ApiError::conflict("会话缺少连接通道"))
}

fn unavailable() -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "连接通道状态暂不可用，请稍后重试")
}

pub(super) async fn get_with_readiness(
    State(state): State<AppState>, headers: HeaderMap, Path(id): Path<String>, Query(query): Query<ReadyQuery>,
) -> Result<Json<SessionReadiness>, ApiError> {
    let session = get_session(State(state.clone()), headers.clone(), Path(id.clone())).await?.0;
    let Some(expected) = query.ready_path else {
        return Ok(Json(SessionReadiness { session, receiver_ready: None }));
    };
    let proxy_name = expected_proxy(&session, expected)?.to_string();
    let Some(integration) = state.diagnostics.read().await.clone() else {
        return Ok(Json(SessionReadiness { session, receiver_ready: None }));
    };
    let kind = if expected == SessionPath::Relay { "stcp" } else { "xtcp" };
    // No application state lock is held across the protected, loopback FRPS query.
    let response = reqwest::Client::builder().no_proxy().timeout(std::time::Duration::from_secs(2))
        .build().map_err(|_| unavailable())?
        .get(format!("{}/api/proxy/{kind}/{proxy_name}", integration.admin_url.trim_end_matches('/')))
        .basic_auth(&integration.admin_user, Some(&integration.admin_password)).send().await
        .map_err(|_| unavailable())?;
    let ready = if response.status() == StatusCode::NOT_FOUND {
        false
    } else {
        let value: serde_json::Value = response.error_for_status().map_err(|_| unavailable())?
            .json().await.map_err(|_| unavailable())?;
        if value["name"].as_str() != Some(proxy_name.as_str()) { return Err(unavailable()); }
        match value["status"].as_str() {
            Some("online") => true,
            Some("offline") => false,
            _ => return Err(unavailable()),
        }
    };
    // A successful admin response can arrive after termination, logout, device
    // revocation, or a direct-to-relay switch. Revalidate before publishing it.
    let current = get_session(State(state), headers, Path(id)).await?.0;
    if expected_proxy(&current, expected)? != proxy_name {
        return Err(ApiError::conflict("连接通道已变化，请重新读取会话"));
    }
    Ok(Json(SessionReadiness { session: current, receiver_ready: Some(ready) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{peer_headers, session_request, test_app};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Notify;

    struct Admin {
        reply: Arc<RwLock<(StatusCode, serde_json::Value)>>,
        calls: Arc<AtomicUsize>,
        kinds: Arc<Mutex<Vec<String>>>,
        block: Arc<AtomicBool>,
        entered: Arc<Notify>,
        release: Arc<Notify>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Admin {
        fn drop(&mut self) { self.task.abort(); }
    }

    async fn admin(state: &AppState, name: &str) -> Admin {
        let reply = Arc::new(RwLock::new((StatusCode::OK, serde_json::json!({"name":name,"status":"online"}))));
        let calls = Arc::new(AtomicUsize::new(0));
        let kinds = Arc::new(Mutex::new(Vec::new()));
        let block = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (reply_mock, calls_mock, kinds_mock) = (reply.clone(), calls.clone(), kinds.clone());
        let (block_mock, entered_mock, release_mock) = (block.clone(), entered.clone(), release.clone());
        let router = Router::new().route("/api/proxy/{kind}/{name}", axum::routing::get(
            move |Path((kind, _)): Path<(String, String)>, headers: HeaderMap| {
                let (reply, calls, kinds) = (reply_mock.clone(), calls_mock.clone(), kinds_mock.clone());
                let (block, entered, release) = (block_mock.clone(), entered_mock.clone(), release_mock.clone());
                async move {
                    if headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()) != Some("Basic YWRtaW46dGVzdA==") {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    calls.fetch_add(1, Ordering::SeqCst);
                    kinds.lock().await.push(kind);
                    entered.notify_one();
                    if block.load(Ordering::SeqCst) { release.notified().await; }
                    let (status, value) = reply.read().await.clone();
                    (status, Json(value)).into_response()
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        state.configure_diagnostics(DiagnosticIntegration { admin_url: format!("http://{address}"),
            admin_user: "admin".into(), admin_password: "test".into(), plugin_token: "fixture".into() }).await;
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        Admin { reply, calls, kinds, block, entered, release, task }
    }

    async fn create(state: &AppState, policy: ConnectionPolicy) -> SessionInfo {
        let mut request = session_request("a", "b");
        request.policy = policy;
        create_session(State(state.clone()), peer_headers("a"), Json(request)).await.unwrap().0
    }

    async fn ready(state: &AppState, id: &str, path: Option<SessionPath>) -> Result<SessionReadiness, ApiError> {
        get_with_readiness(State(state.clone()), peer_headers("a"), Path(id.into()),
            Query(ReadyQuery { ready_path: path })).await.map(|response| response.0)
    }

    #[tokio::test]
    async fn legacy_json_and_unmanaged_centers_remain_compatible() {
        let app = test_app();
        let session = create(&app.state, ConnectionPolicy::DirectPreferred).await;
        let original = serde_json::to_value(&session).unwrap();
        let legacy: SessionReadiness = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(legacy.receiver_ready, None);
        for path in [None, Some(SessionPath::Direct)] {
            let response = ready(&app.state, &session.session_id, path).await.unwrap();
            assert_eq!(response.receiver_ready, None);
            assert_eq!(serde_json::to_value(response).unwrap(), original);
        }
        let response = SessionReadiness { session: session.clone(), receiver_ready: Some(true) };
        let old_client: SessionInfo = serde_json::from_value(serde_json::to_value(response).unwrap()).unwrap();
        assert_eq!(old_client.session_id, session.session_id);
    }

    #[tokio::test]
    async fn waits_for_the_current_named_proxy_to_be_online() {
        let app = test_app();
        let session = create(&app.state, ConnectionPolicy::DirectOnly).await;
        let name = &session.route.as_ref().unwrap().proxy_name;
        let admin = admin(&app.state, name).await;
        *admin.reply.write().await = (StatusCode::NOT_FOUND, serde_json::json!({}));
        assert_eq!(ready(&app.state, &session.session_id, Some(SessionPath::Direct)).await.unwrap().receiver_ready, Some(false));
        *admin.reply.write().await = (StatusCode::OK, serde_json::json!({"name":name,"status":"offline"}));
        assert_eq!(ready(&app.state, &session.session_id, Some(SessionPath::Direct)).await.unwrap().receiver_ready, Some(false));
        *admin.reply.write().await = (StatusCode::OK, serde_json::json!({"name":name,"status":"online"}));
        assert_eq!(ready(&app.state, &session.session_id, Some(SessionPath::Direct)).await.unwrap().receiver_ready, Some(true));
        assert_eq!(*admin.kinds.lock().await, ["xtcp", "xtcp", "xtcp"]);
        assert_eq!(ready(&app.state, &session.session_id, Some(SessionPath::Relay)).await.unwrap_err().status, StatusCode::CONFLICT);
        assert_eq!(admin.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn relay_has_a_separate_stcp_readiness_gate() {
        let app = test_app();
        let session = create(&app.state, ConnectionPolicy::RelayOnly).await;
        let admin = admin(&app.state, &session.route.as_ref().unwrap().proxy_name).await;
        assert_eq!(ready(&app.state, &session.session_id, Some(SessionPath::Relay)).await.unwrap().receiver_ready, Some(true));
        assert_eq!(*admin.kinds.lock().await, ["stcp"]);
        assert!(ready(&app.state, &session.session_id, Some(SessionPath::Direct)).await.is_err());
    }

    #[tokio::test]
    async fn rejects_nonparticipants_and_bad_credentials_before_querying_frps() {
        let app = test_app();
        let session = create(&app.state, ConnectionPolicy::DirectPreferred).await;
        let admin = admin(&app.state, &session.route.as_ref().unwrap().proxy_name).await;
        let mut bad_token = peer_headers("a");
        bad_token.insert("x-starbridge-device-token", "wrong".parse().unwrap());
        let mut foreign_account = peer_headers("z");
        foreign_account.insert(header::AUTHORIZATION, "Bearer bob-access".parse().unwrap());
        for headers in [HeaderMap::new(), peer_headers("c"), foreign_account, bad_token] {
            assert!(get_with_readiness(State(app.state.clone()), headers, Path(session.session_id.clone()),
                Query(ReadyQuery { ready_path: Some(SessionPath::Direct) })).await.is_err());
        }
        assert_eq!(admin.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn closed_sessions_cannot_publish_readiness() {
        let app = test_app();
        let session = create(&app.state, ConnectionPolicy::DirectPreferred).await;
        let admin = admin(&app.state, &session.route.as_ref().unwrap().proxy_name).await;
        let _ = delete_session(State(app.state.clone()), peer_headers("a"), Path(session.session_id.clone())).await.unwrap();
        assert_eq!(ready(&app.state, &session.session_id, Some(SessionPath::Direct)).await.unwrap_err().status, StatusCode::CONFLICT);
        assert_eq!(admin.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn in_flight_close_or_mode_change_cannot_publish_stale_readiness() {
        for mode_change in [false, true] {
            let app = test_app();
            let session = create(&app.state, ConnectionPolicy::DirectPreferred).await;
            let admin = admin(&app.state, &session.route.as_ref().unwrap().proxy_name).await;
            admin.block.store(true, Ordering::SeqCst);
            let state = app.state.clone(); let id = session.session_id.clone();
            let request = tokio::spawn(async move { ready(&state, &id, Some(SessionPath::Direct)).await });
            admin.entered.notified().await;
            {
                let mut inner = app.state.inner.write().await;
                let current = inner.sessions.get_mut(&session.session_id).unwrap();
                if mode_change { current.path = SessionPath::Relay; } else { current.status = SessionStatus::Closed; }
            }
            admin.release.notify_one();
            assert_eq!(request.await.unwrap().unwrap_err().status, StatusCode::CONFLICT);
        }
    }

    #[tokio::test]
    async fn in_flight_proxy_change_or_auth_revocation_cannot_publish_stale_readiness() {
        for auth_revoked in [false, true] {
            let app = test_app();
            let session = create(&app.state, ConnectionPolicy::DirectPreferred).await;
            let admin = admin(&app.state, &session.route.as_ref().unwrap().proxy_name).await;
            admin.block.store(true, Ordering::SeqCst);
            let state = app.state.clone(); let id = session.session_id.clone();
            let request = tokio::spawn(async move { ready(&state, &id, Some(SessionPath::Direct)).await });
            admin.entered.notified().await;
            {
                let mut inner = app.state.inner.write().await;
                if auth_revoked { inner.tokens.remove("alice-access"); }
                else { inner.sessions.get_mut(&session.session_id).unwrap().route.as_mut().unwrap().proxy_name = "new-route".into(); }
            }
            admin.release.notify_one();
            assert!(request.await.unwrap().is_err());
        }
    }

    #[tokio::test]
    async fn missing_or_invalid_admin_evidence_is_not_ready() {
        let app = test_app();
        let session = create(&app.state, ConnectionPolicy::DirectPreferred).await;
        let admin = admin(&app.state, &session.route.as_ref().unwrap().proxy_name).await;
        for reply in [(StatusCode::INTERNAL_SERVER_ERROR, serde_json::json!({})),
            (StatusCode::OK, serde_json::json!({"name":"another-session","status":"online"})),
            (StatusCode::OK, serde_json::json!({"name":session.route.as_ref().unwrap().proxy_name,"status":"unknown"}))] {
            *admin.reply.write().await = reply;
            assert_eq!(ready(&app.state, &session.session_id, Some(SessionPath::Direct)).await.unwrap_err().status,
                StatusCode::SERVICE_UNAVAILABLE);
        }
    }
}
