use super::*;

pub(super) async fn connection_history(
    State(app): State<ClientApp>,
    Json(form): Json<SessionHistoryQuery>,
) -> Result<Json<SessionHistoryPage>, ApiError> {
    let generation = app.inner.read().await.auth_generation;
    app.auth_context().await?;
    app.refresh_auth(false).await.map_err(ApiError::internal)?;
    let (auth, url, identity) = app.auth_context().await?;
    {
        let inner = app.inner.read().await;
        if inner.auth_generation != generation || inner.closing_account {
            return Err(ApiError::bad_request("登录状态已更改，请重新读取连接记录"));
        }
    }
    let response = app.http.get(format!("{url}/api/v1/sessions/history/{}", identity.node_id))
        .bearer_auth(auth.access_token)
        .header("x-starbridge-device-token", &identity.device_token)
        .query(&form)
        .send().await.map_err(ApiError::network)?;
    if response.status() == StatusCode::NOT_FOUND {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "请升级中心服务后查看连接记录"));
    }
    let page: SessionHistoryPage = response_json(response).await?;
    let inner = app.inner.read().await;
    if inner.auth_generation != generation || inner.closing_account
        || inner.identity.node_id != identity.node_id || inner.identity.device_token != identity.device_token
        || inner.config.server_url != url || inner.auth.is_none() {
        return Err(ApiError::bad_request("登录状态已更改，请重新读取连接记录"));
    }
    Ok(Json(page))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Query, http::HeaderMap};

    async fn fixture(router: Router) -> (ClientApp, PathBuf, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        let root = std::env::temp_dir().join(format!("starbridge-history-{}", uuid::Uuid::new_v4()));
        let app = ClientApp::load(Role::Peer, root.clone()).await.unwrap();
        {
            let mut inner = app.inner.write().await;
            inner.config.server_url = url;
            inner.identity.node_id = "local-node".into();
            inner.identity.device_token = "device-secret".into();
            inner.auth = Some(AuthResponse {
                username: "alice".into(), access_token: "account-secret".into(),
                expires_at: now_unix() + 3600, refresh_token: None, refresh_expires_at: None,
            });
        }
        (app, root, server)
    }

    async fn cleanup(app: ClientApp, root: PathBuf, server: tokio::task::JoinHandle<()>) {
        app.inner.write().await.auth = None;
        app.shutdown().await;
        server.abort();
        let _ = server.await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn history_bridge_authenticates_and_does_not_export_route_secrets() {
        let router = Router::new().route("/api/v1/sessions/history/{node}", get(
            |Path(node): Path<String>, Query(query): Query<SessionHistoryQuery>, headers: HeaderMap| async move {
                assert_eq!(node, "local-node");
                assert_eq!(query.offset, 50);
                assert_eq!(headers["authorization"], "Bearer account-secret");
                assert_eq!(headers["x-starbridge-device-token"], "device-secret");
                Json(serde_json::json!({"total":51,"items":[{
                    "session_id":"history-1","peer_name":"工作电脑","direction":"incoming",
                    "status":"closed","path":"direct","created_at":123,"failure_reason":null,
                    "route":{"auth_token":"route-secret","secret_key":"frp-secret"},
                    "ssh_command":"obsolete-command"
                }]}))
            }
        ));
        let (app, root, server) = fixture(router).await;
        let page = connection_history(State(app.clone()), Json(SessionHistoryQuery { offset: 50 })).await.unwrap().0;
        assert_eq!(page.total, 51);
        assert_eq!(page.items[0].peer_name, "工作电脑");
        let encoded = serde_json::to_string(&page).unwrap();
        for forbidden in ["route-secret", "frp-secret", "obsolete-command", "account-secret", "device-secret"] {
            assert!(!encoded.contains(forbidden));
        }
        cleanup(app, root, server).await;
    }

    #[tokio::test]
    async fn history_bridge_rejects_response_after_account_change() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let request_entered = entered.clone();
        let response_release = release.clone();
        let router = Router::new().route("/api/v1/sessions/history/{node}", get(move || {
            let entered = request_entered.clone();
            let release = response_release.clone();
            async move {
                entered.notify_one();
                release.notified().await;
                Json(serde_json::json!({"total":0,"items":[]}))
            }
        }));
        let (app, root, server) = fixture(router).await;
        let requester = app.clone();
        let request = tokio::spawn(async move {
            connection_history(State(requester), Json(SessionHistoryQuery { offset: 0 })).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified()).await.unwrap();
        app.inner.write().await.auth_generation += 1;
        release.notify_one();
        let error = request.await.unwrap().unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert!(error.message.contains("登录状态已更改"));
        cleanup(app, root, server).await;
    }

    #[tokio::test]
    async fn history_bridge_reports_missing_login_and_server_upgrade() {
        let (app, root, server) = fixture(Router::new()).await;
        let error = connection_history(State(app.clone()), Json(SessionHistoryQuery { offset: 0 })).await.unwrap_err();
        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert!(error.message.contains("升级中心服务"));
        app.inner.write().await.auth = None;
        let error = connection_history(State(app.clone()), Json(SessionHistoryQuery { offset: 0 })).await.unwrap_err();
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);
        cleanup(app, root, server).await;
    }
}
