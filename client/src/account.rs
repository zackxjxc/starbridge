use super::*;

#[derive(Debug, Deserialize)]
pub struct RegisterForm {
    pub username: String,
    pub password: String,
    pub confirm_password: String,
    #[serde(default = "default_true")]
    pub remember: bool,
}

#[derive(Debug, Deserialize)]
pub struct DeviceMetadataForm {
    pub node_id: String,
    pub display_name: String,
    #[serde(default)]
    pub purpose: String,
}

#[derive(Debug, Deserialize)]
pub struct DeviceDeleteForm {
    pub node_id: String,
}

#[derive(Deserialize)]
pub struct DeviceRebindForm {
    pub password: String,
}

pub(super) fn account_scope(server: &str, username: &str) -> String {
    // Length-prefix the server component so distinct pairs cannot alias.
    let server = server.trim().trim_end_matches('/');
    format!("{}:{}{}", server.len(), server, username.trim())
}

#[derive(Debug)]
struct RegistrationError { status: StatusCode, message: String }

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.message) }
}
impl std::error::Error for RegistrationError {}

pub(super) fn registration_status(error: &anyhow::Error) -> Option<StatusCode> {
    error.downcast_ref::<RegistrationError>().map(|error| error.status)
        .or_else(|| error.downcast_ref::<reqwest::Error>().and_then(reqwest::Error::status))
}

async fn registration_response(response: reqwest::Response) -> anyhow::Result<DeviceRegistrationResponse> {
    let status = response.status();
    if !status.is_success() {
        // Never display response bodies or request identifiers in a device-authentication error.
        let message = match status {
            StatusCode::CONFLICT => "设备身份存在冲突。请确认是否为本机的旧记录，再选择重新绑定；克隆电脑或独立测试请使用独立数据目录。",
            StatusCode::GONE => "此设备已被删除。请登录账号后明确重新绑定；后台不会重新添加。",
            StatusCode::UNAUTHORIZED => "账号或设备验证失败，请重新登录；重新绑定需要再次验证账号密码。",
            StatusCode::FORBIDDEN => "此设备凭据已失效，请登录账号后明确重新绑定。",
            _ => "设备登记失败，请稍后重试或检查服务状态。",
        };
        return Err(RegistrationError { status, message: message.into() }.into());
    }
    Ok(response.json().await?)
}

pub(super) async fn response_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, ApiError> {
    let status = response.status();
    if !status.is_success() {
        let message = response.json::<serde_json::Value>().await.ok()
            .and_then(|value| value.get("error").and_then(|value| value.as_str()).map(str::to_owned))
            .unwrap_or_else(|| format!("服务器返回 {status}"));
        return Err(ApiError::new(status, message));
    }
    response.json::<T>().await.map_err(ApiError::network)
}

pub(super) async fn login(
    State(app): State<ClientApp>, Json(form): Json<LoginForm>,
) -> Result<Json<ClientSnapshot>, ApiError> {
    authenticate_account(&app, form, false).await
}

pub(super) async fn register(
    State(app): State<ClientApp>, Json(form): Json<RegisterForm>,
) -> Result<Json<ClientSnapshot>, ApiError> {
    if form.password != form.confirm_password {
        return Err(ApiError::bad_request("两次输入的密码不一致"));
    }
    authenticate_account(&app, LoginForm {
        username: form.username, password: form.password, remember: form.remember,
    }, true).await
}

async fn authenticate_account(app: &ClientApp, form: LoginForm, registering: bool)
    -> Result<Json<ClientSnapshot>, ApiError>
{
    if form.username.trim().is_empty() || form.password.len() < 4 {
        return Err(ApiError::bad_request("请输入账户名称和至少 4 位密码"));
    }
    let _heartbeat = app.heartbeat_gate.lock().await;
    let _gate = app.session_operations.lock().await;
    let (url, generation) = {
        let mut inner = app.inner.write().await;
        if inner.auth.as_ref().is_some_and(|auth| auth.username != form.username.trim()) {
            return Err(ApiError::bad_request("请先退出当前账号，再登录其他账号"));
        }
        inner.auth_generation += 1;
        inner.config.last_username = form.username.trim().to_string();
        inner.config.remember_login = form.remember;
        save_json(&inner.root.join("config.json"), &inner.config).await.map_err(ApiError::internal)?;
        clear_saved_auth(&inner.root).await;
        (inner.config.server_url.clone(), inner.auth_generation)
    };
    let endpoint = if registering { "register" } else { "login" };
    let response = app.http.post(format!("{url}/api/v1/auth/{endpoint}"))
        .json(&AuthRequest { username: form.username.trim().into(), password: form.password })
        .send().await.map_err(ApiError::network)?;
    let auth: AuthResponse = response_json(response).await?;
    // Resolve ownership of an unclaimed portable identity using the authenticated
    // account's list before choosing a credential. A different first account must
    // never consume or reuse the original account's credential.
    let owned: Vec<DeviceInfo> = response_json(app.http.get(format!("{url}/api/v1/devices"))
        .bearer_auth(&auth.access_token).send().await.map_err(ApiError::network)?).await?;
    let owned_nodes = owned.into_iter().map(|device| device.node_id).collect::<Vec<_>>();
    {
        let mut inner = app.inner.write().await;
        if generation != inner.auth_generation || app.shutting_down.load(Ordering::Acquire) {
            return Err(ApiError::bad_request("登录已取消"));
        }
        let mut selected = inner.identity.clone();
        selected.select_account_for_devices(&account_scope(&inner.config.server_url, &auth.username), &owned_nodes);
        identity::save_identity(&inner.root, &selected).await.map_err(ApiError::internal)?;
        inner.identity = selected;
        inner.auth = Some(auth.clone());
        inner.device_rebind_required = false;
        inner.stage = "online".into();
        inner.last_error = None;
        if form.remember { save_auth(&inner.root, &auth).await.map_err(ApiError::internal)?; }
    }
    drop(_gate);
    if let Err(error) = app.refresh_devices().await {
        let mut inner = app.inner.write().await;
        inner.device_rebind_required = matches!(registration_status(&error),
            Some(StatusCode::CONFLICT | StatusCode::GONE | StatusCode::FORBIDDEN));
        inner.stage = "device_verification_required".into();
        inner.last_error = Some(error.to_string());
    }
    Ok(Json(snapshot(&*app.inner.read().await)))
}

/// Called only by the explicit password-confirmation UI, never by login or heartbeat.
pub(super) async fn rebind_device(
    State(app): State<ClientApp>, Json(form): Json<DeviceRebindForm>,
) -> Result<Json<ClientSnapshot>, ApiError> {
    if form.password.is_empty() { return Err(ApiError::bad_request("请输入账号密码以确认重新绑定")); }
    let _heartbeat = app.heartbeat_gate.lock().await;
    let _gate = app.session_operations.lock().await;
    let _updates = app.device_updates.lock().await;
    let (auth, url, registration, generation) = {
        let mut inner = app.inner.write().await;
        if inner.closing_account || !inner.device_rebind_required {
            return Err(ApiError::bad_request("当前没有需要重新绑定的设备，请先登录并检查设备状态"));
        }
        if inner.sessions.values().any(|session| matches!(session.status, SessionStatus::Negotiating | SessionStatus::Connected)) {
            return Err(ApiError::bad_request("请先终止连接，再重新绑定设备"));
        }
        let auth = inner.auth.clone().ok_or_else(|| ApiError::unauthorized("请先登录账号"))?;
        let mut pending = inner.identity.clone();
        pending.pending_rebind_token.get_or_insert_with(new_token);
        // Save before the network call: a timeout or crash can retry the same credential.
        identity::save_identity(&inner.root, &pending).await.map_err(ApiError::internal)?;
        inner.identity = pending;
        (auth, inner.config.server_url.clone(), device_registration(&inner), inner.auth_generation)
    };
    let response = app.http.post(format!("{url}/api/v1/devices/rebind"))
        .bearer_auth(&auth.access_token)
        .json(&DeviceRebindRequest { registration, auth: AuthRequest { username: auth.username, password: form.password } })
        .send().await.map_err(ApiError::network)?;
    let result = registration_response(response).await.map_err(|error|
        ApiError::new(registration_status(&error).unwrap_or(StatusCode::BAD_GATEWAY), error.to_string()))?;
    app.accept_registration(result, generation).await.map_err(ApiError::internal)?;
    Ok(Json(snapshot(&*app.inner.read().await)))
}

pub(super) async fn logout(State(app): State<ClientApp>) -> Result<Json<ClientSnapshot>, ApiError> {
    logout_account(&app, None, None).await
}

pub(super) async fn logout_removed_device(app: &ClientApp, generation: u64) -> Result<Json<ClientSnapshot>, ApiError> {
    logout_account(app, Some(generation), Some("此设备已从账号删除，请重新登录并明确重新绑定后添加。")).await
}

async fn logout_account(app: &ClientApp, expected_generation: Option<u64>, reason: Option<&str>) -> Result<Json<ClientSnapshot>, ApiError> {
    let _gate=app.session_operations.lock().await;
    {
        let mut inner=app.inner.write().await;
        if expected_generation.is_some_and(|generation| generation != inner.auth_generation) {
            return Ok(Json(snapshot(&inner)));
        }
        inner.closing_account=true;inner.auth_generation+=1;inner.stage="signing_out".into();
    }
    app.diagnostic_cancel.store(true, Ordering::Release);
    app.terminate_targets(None, true).await;
    // Local listeners stop first; keep credentials briefly so revocation can complete.
    for _ in 0..80 {
        if !matches!(app.inner.read().await.self_test.state.as_str(), "running" | "cleaning") { break; }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        app.retry_pending_closes().await;
        app.mark_offline().await;
    }).await;
    let (auth, url) = {
        let mut inner = app.inner.write().await;
        inner.auth_generation += 1;
        let auth = inner.auth.take();
        inner.closing_account=false;
        inner.devices.clear();
        inner.device_rebind_required = false;
        inner.stage = "ready".into();
        inner.last_error = reason.map(str::to_owned);
        clear_saved_auth(&inner.root).await;
        (auth, inner.config.server_url.clone())
    };
    let _initialization = app.initialization.lock().await;
    app.stop_sshd().await;
    app.inner.write().await.ssh = SshStatus { initialization: "not_initialized".into(), ..Default::default() };
    drop(_initialization);
    if let Some(auth) = auth {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), app.http
            .post(format!("{url}/api/v1/auth/logout"))
            .bearer_auth(auth.access_token)
            .json(&serde_json::json!({"refresh_token":auth.refresh_token}))
            .send()).await;
    }
    Ok(Json(snapshot(&*app.inner.read().await)))
}

pub(super) async fn delete_device(
    State(app): State<ClientApp>, Json(form): Json<DeviceDeleteForm>,
) -> Result<Json<ClientSnapshot>, ApiError> {
    app.refresh_auth(false).await.map_err(ApiError::internal)?;
    // Match heartbeat's lock order and exclude an in-flight registration response.
    let heartbeat = app.heartbeat_gate.lock().await;
    let gate = app.session_operations.lock().await;
    let updates = app.device_updates.lock().await;
    let (auth, url, identity) = app.auth_context().await?;
    let generation = app.inner.read().await.auth_generation;
    let local = identity.node_id == form.node_id;
    {
        let inner = app.inner.read().await;
        if inner.closing_account { return Err(ApiError::bad_request("正在退出登录")); }
        if !local && !inner.devices.iter().any(|item| item.node_id == form.node_id) {
            return Err(ApiError::bad_request("设备已不在账号列表中"));
        }
        let pending = inner.operations.values().any(|item| (local || item.node_id == form.node_id)
            && matches!(item.state.as_str(), "queued" | "connecting" | "verifying"));
        let connected = inner.sessions.values().any(|item| matches!(item.status, SessionStatus::Negotiating | SessionStatus::Connected)
            && (local || item.initiator_node_id == form.node_id || item.target_node_id == form.node_id));
        if pending || connected || (local && matches!(inner.self_test.state.as_str(), "running" | "cleaning")) {
            return Err(ApiError::bad_request("请先终止该设备的连接、等待任务或连接测试，再删除设备"));
        }
    }
    let response = app.http.delete(format!("{url}/api/v1/devices/{}", form.node_id))
        .bearer_auth(auth.access_token)
        .header("x-starbridge-node-id", &identity.node_id)
        .header("x-starbridge-device-token", &identity.device_token)
        .send().await.map_err(ApiError::network)?;
    let devices: Vec<DeviceInfo> = response_json(response).await?;
    {
        let mut inner = app.inner.write().await;
        if inner.auth_generation != generation { return Err(ApiError::bad_request("登录状态已更改")); }
        inner.devices = devices.clone();
        inner.operations.remove(&form.node_id);
        if let Some(batch) = inner.batch.as_mut() { batch.targets.retain(|id| id != &form.node_id); }
    }
    app.sync_authorized_keys(&devices).await;
    drop(updates);
    drop(gate);
    drop(heartbeat);
    if local { return logout_removed_device(&app, generation).await; }
    Ok(Json(snapshot(&*app.inner.read().await)))
}

pub(super) async fn update_device(
    State(app): State<ClientApp>, Json(form): Json<DeviceMetadataForm>,
) -> Result<Json<ClientSnapshot>, ApiError> {
    let _updates = app.device_updates.lock().await;
    let name = form.display_name.trim();
    if name.is_empty() || name.chars().count() > 80 || form.purpose.chars().count() > 240 {
        return Err(ApiError::bad_request("名称不能为空且最多 80 字，用途最多 240 字"));
    }
    let (auth, url, node) = app.auth_context().await?;
    let response = app.http.put(format!("{url}/api/v1/devices/{}", form.node_id))
        .bearer_auth(auth.access_token)
        .json(&serde_json::json!({"display_name":name,"purpose":form.purpose.trim()}))
        .send().await.map_err(ApiError::network)?;
    let devices: Vec<DeviceInfo> = response_json(response).await?;
    let mut inner = app.inner.write().await;
    inner.devices = devices;
    if node.node_id == form.node_id {
        inner.config.display_name = name.to_string();
        inner.config.purpose = form.purpose.trim().to_string();
        save_json(&inner.root.join("config.json"), &inner.config).await.map_err(ApiError::internal)?;
    }
    Ok(Json(snapshot(&inner)))
}

impl ClientApp {
    pub(super) async fn auth_context(&self) -> Result<(AuthResponse, String, Identity), ApiError> {
        let inner = self.inner.read().await;
        Ok((inner.auth.clone().ok_or_else(|| ApiError::unauthorized("请先登录"))?,
            inner.config.server_url.clone(), inner.identity.clone()))
    }

    pub(super) async fn refresh_auth(&self, force: bool) -> anyhow::Result<()> {
        self.refresh_auth_matching(force, None).await
    }

    pub(super) async fn refresh_auth_matching(&self, force: bool, expected: Option<(u64, &str)>) -> anyhow::Result<()> {
        let _gate=self.session_operations.lock().await;
        let (auth, url, generation) = {
            let inner = self.inner.read().await;
            if inner.closing_account { anyhow::bail!("正在退出登录"); }
            if expected.is_some_and(|(generation, _)| generation != inner.auth_generation) {
                anyhow::bail!("登录状态已更改");
            }
            let auth = inner.auth.clone().ok_or_else(|| anyhow::anyhow!("请先登录"))?;
            // Another protected request may already have rotated this rejected token.
            if expected.is_some_and(|(_, token)| token != auth.access_token) { return Ok(()); }
            (auth, inner.config.server_url.clone(), inner.auth_generation)
        };
        if !force && auth.expires_at > now_unix() + 60 { return Ok(()); }
        let refresh = auth.refresh_token.as_ref().ok_or_else(|| anyhow::anyhow!("登录已过期，请重新登录"))?;
        let response = self.http.post(format!("{url}/api/v1/auth/refresh"))
            .json(&serde_json::json!({"refresh_token":refresh})).send().await?.error_for_status()?;
        let updated: AuthResponse = response.json().await?;
        let mut inner = self.inner.write().await;
        if inner.auth_generation != generation || inner.closing_account
            || inner.auth.as_ref().map(|value| &value.access_token) != Some(&auth.access_token) {
            anyhow::bail!("登录状态已更改");
        }
        if inner.config.remember_login { save_auth(&inner.root, &updated).await?; }
        inner.auth = Some(updated);
        Ok(())
    }

    pub(super) async fn refresh_devices(&self) -> anyhow::Result<Vec<DeviceInfo>> {
        let _updates = self.device_updates.lock().await;
        let (auth, url, generation, registration) = {
            let inner = self.inner.read().await;
            if inner.closing_account { anyhow::bail!("正在退出登录"); }
            (inner.auth.clone().ok_or_else(|| anyhow::anyhow!("未登录"))?,
                inner.config.server_url.clone(), inner.auth_generation, device_registration(&inner))
        };
        let response = self.http.post(format!("{url}/api/v1/devices/identify"))
            .bearer_auth(auth.access_token).json(&registration).send().await?;
        let result = registration_response(response).await?;
        self.accept_registration(result, generation).await
    }

    async fn accept_registration(&self, result: DeviceRegistrationResponse, generation: u64) -> anyhow::Result<Vec<DeviceInfo>> {
        let devices = result.devices;
        if self.inner.read().await.auth_generation != generation { anyhow::bail!("登录状态已更改"); }
        let mut inner = self.inner.write().await;
        if inner.auth_generation != generation { anyhow::bail!("登录状态已更改"); }
        let mut canonical = inner.identity.clone();
        canonical.node_id = result.node_id;
        if let Some(token) = canonical.pending_rebind_token.take() { canonical.device_token = token; }
        if canonical.node_id != inner.identity.node_id || inner.identity.pending_rebind_token.is_some() {
            identity::save_identity(&inner.root, &canonical).await?;
        }
        inner.identity = canonical;
        inner.device_rebind_required = false;
        inner.last_error = None;
        inner.stage = "online".into();
        if let Some(local) = devices.iter().find(|device| device.node_id == inner.identity.node_id) {
            if inner.config.display_name != local.display_name || inner.config.purpose != local.purpose {
                inner.config.display_name = local.display_name.clone();
                inner.config.purpose = local.purpose.clone();
                save_json(&inner.root.join("config.json"), &inner.config).await?;
            }
        }
        inner.devices = devices.clone();
        drop(inner);
        self.sync_authorized_keys(&devices).await;
        Ok(devices)
    }
}

pub(super) async fn clear_saved_auth(root: &FsPath) {
    let _ = secure_store::remove_secret(root, "auth").await;
    let _ = tokio::fs::remove_file(root.join("auth.json")).await;
}

pub(super) async fn load_auth(root: &FsPath, remember: bool) -> anyhow::Result<Option<AuthResponse>> {
    if !remember { clear_saved_auth(root).await; return Ok(None); }
    let bytes = match secure_store::read_secret(root, "auth").await? {
        Some(bytes) => bytes,
        None => match tokio::fs::read(root.join("auth.json")).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => anyhow::bail!("无法读取已保存的登录状态"),
        },
    };
    let auth: Option<AuthResponse> = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("已保存的登录状态无法读取，请重新登录"))?;
    if let Some(auth) = &auth { save_auth(root, auth).await?; }
    Ok(auth.filter(|auth| auth.expires_at > now_unix()
        || auth.refresh_expires_at.is_some_and(|expiry| expiry > now_unix())))
}

pub(super) async fn save_auth(root: &FsPath, auth: &AuthResponse) -> anyhow::Result<()> {
    secure_store::write_secret(root, "auth", &serde_json::to_vec(auth)?).await?;
    match tokio::fs::remove_file(root.join("auth.json")).await {
        Ok(()) => {},
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(_) => anyhow::bail!("登录状态已安全保存，但旧登录文件无法移除，请检查数据目录权限"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        url: String,
        server: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        async fn new() -> Self {
            let root = std::env::temp_dir().join(format!("starbridge-machine-e2e-{}", uuid::Uuid::new_v4()));
            let state = starbridge_server::AppState::with_frp_and_data("127.0.0.1".into(), 17851, "test".into(), root.join("server"));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, starbridge_server::app(state)).await.unwrap(); });
            let fixture = Self { root, url, server };
            for username in ["alice", "bob"] {
                reqwest::Client::new().post(format!("{}/api/v1/auth/register", fixture.url))
                    .json(&AuthRequest { username: username.into(), password: "test-password".into() })
                    .send().await.unwrap().error_for_status().unwrap();
            }
            fixture
        }

        async fn client(&self, name: &str) -> ClientApp {
            let root = self.root.join(name);
            let app = ClientApp::load(Role::Peer, root).await.unwrap();
            {
                let mut inner = app.inner.write().await;
                inner.config.server_url = self.url.clone();
                save_json(&inner.root.join("config.json"), &inner.config).await.unwrap();
            }
            app
        }

        async fn cleanup(self, clients: Vec<ClientApp>) {
            for client in clients { client.shutdown().await; }
            self.server.abort();
            let _ = self.server.await;
            std::fs::remove_dir_all(self.root).unwrap();
        }
    }

    async fn sign_in(app: &ClientApp, username: &str) -> ClientSnapshot {
        login(State(app.clone()), Json(LoginForm { username: username.into(), password: "test-password".into(), remember: true }))
            .await.unwrap().0
    }

    async fn same_machine(source: &ClientApp, target: &ClientApp) {
        let source = source.inner.read().await;
        let mut target = target.inner.write().await;
        target.identity.machine_key = source.identity.machine_key.clone();
        tokio::fs::copy(source.root.join("machine.json"), target.root.join("machine.json")).await.unwrap();
    }

    #[tokio::test]
    async fn repeated_login_restart_and_account_switch_keep_separate_stable_devices() {
        let fixture = Fixture::new().await;
        let app = fixture.client("shared-user-data").await;
        let first = sign_in(&app, "alice").await;
        assert_eq!(first.devices.len(), 1);
        let repeated = sign_in(&app, "alice").await;
        assert_eq!(repeated.node_id, first.node_id);
        assert_eq!(repeated.devices.len(), 1);
        let root = app.inner.read().await.root.clone();
        app.shutdown().await;
        let restarted = ClientApp::load(Role::Peer, root).await.unwrap();
        let devices = restarted.refresh_devices().await.unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(restarted.inner.read().await.identity.node_id, first.node_id);
        let _ = logout(State(restarted.clone())).await.unwrap();
        let bob = sign_in(&restarted, "bob").await;
        assert_eq!(bob.devices.len(), 1);
        assert_ne!(bob.node_id, first.node_id);
        let _ = logout(State(restarted.clone())).await.unwrap();
        let alice_again = sign_in(&restarted, "alice").await;
        assert_eq!(alice_again.node_id, first.node_id);
        assert!(!alice_again.device_rebind_required);
        fixture.cleanup(vec![restarted]).await;
    }

    #[tokio::test]
    async fn credential_conflict_needs_explicit_password_rebind_and_revokes_previous_credential() {
        let fixture = Fixture::new().await;
        let original = fixture.client("original").await;
        let first = sign_in(&original, "alice").await;
        let replacement = fixture.client("reextracted-without-credentials").await;
        same_machine(&original, &replacement).await;
        let conflict = sign_in(&replacement, "alice").await;
        assert!(conflict.device_rebind_required);
        assert_eq!(original.refresh_devices().await.unwrap().len(), 1);
        for _ in 0..2 { assert!(replacement.heartbeat_once().await.is_err()); }
        assert_eq!(original.refresh_devices().await.unwrap().len(), 1);
        let wrong = rebind_device(State(replacement.clone()), Json(DeviceRebindForm { password: "wrong".into() })).await;
        assert!(wrong.is_err());
        assert!(original.refresh_devices().await.is_ok());
        let rebound = rebind_device(State(replacement.clone()), Json(DeviceRebindForm { password: "test-password".into() })).await.unwrap().0;
        assert_eq!(rebound.node_id, first.node_id);
        assert_eq!(rebound.devices.len(), 1);
        assert!(!rebound.device_rebind_required);
        assert!(original.refresh_devices().await.is_err());
        assert_eq!(replacement.refresh_devices().await.unwrap().len(), 1);
        fixture.cleanup(vec![original, replacement]).await;
    }

    #[tokio::test]
    async fn deletion_stays_deleted_through_login_and_background_retry_until_explicit_rebind() {
        let fixture = Fixture::new().await;
        let app = fixture.client("deleted-client").await;
        let first = sign_in(&app, "alice").await;
        let _ = delete_device(State(app.clone()), Json(DeviceDeleteForm { node_id: first.node_id.clone() })).await.unwrap();
        assert!(app.heartbeat_once().await.is_err());
        let signed_in = sign_in(&app, "alice").await;
        assert!(signed_in.device_rebind_required);
        for _ in 0..2 {
            let error = app.heartbeat_once().await.unwrap_err();
            assert_eq!(registration_status(&error), Some(StatusCode::GONE));
        }
        let rebound = rebind_device(State(app.clone()), Json(DeviceRebindForm { password: "test-password".into() })).await.unwrap().0;
        assert_eq!(rebound.node_id, first.node_id);
        assert_eq!(rebound.devices.len(), 1);
        fixture.cleanup(vec![app]).await;
    }

    #[tokio::test]
    async fn parallel_registration_cannot_replace_another_instance_and_distinct_machines_register() {
        let fixture = Fixture::new().await;
        let first = fixture.client("first").await;
        let second = fixture.client("second").await;
        same_machine(&first, &second).await;
        let (a, b) = tokio::join!(sign_in(&first, "alice"), sign_in(&second, "alice"));
        assert_ne!(a.device_rebind_required, b.device_rebind_required);
        let winner = if a.device_rebind_required { &second } else { &first };
        assert_eq!(winner.refresh_devices().await.unwrap().len(), 1);
        let different = fixture.client("another-machine").await;
        different.inner.write().await.identity.machine_key = "c".repeat(64);
        let result = sign_in(&different, "alice").await;
        assert!(!result.device_rebind_required);
        assert_eq!(result.devices.len(), 2);
        fixture.cleanup(vec![first, second, different]).await;
    }

    #[tokio::test]
    async fn lost_rebind_response_recovers_pending_credential_after_restart_without_another_rebind() {
        let fixture = Fixture::new().await;
        let original = fixture.client("old-install").await;
        let first = sign_in(&original, "alice").await;
        let replacement = fixture.client("new-install").await;
        same_machine(&original, &replacement).await;
        assert!(sign_in(&replacement, "alice").await.device_rebind_required);
        let (registration, auth, root) = {
            let mut inner = replacement.inner.write().await;
            inner.identity.pending_rebind_token = Some(new_token());
            identity::save_identity(&inner.root, &inner.identity).await.unwrap();
            (device_registration(&inner), inner.auth.clone().unwrap(), inner.root.clone())
        };
        // The server commits, but the client never consumes the canonical response.
        fixture.client("unused").await.http.post(format!("{}/api/v1/devices/rebind", fixture.url))
            .bearer_auth(auth.access_token)
            .json(&DeviceRebindRequest { registration, auth: AuthRequest { username: "alice".into(), password: "test-password".into() } })
            .send().await.unwrap().error_for_status().unwrap();
        replacement.shutdown().await;
        let restarted = ClientApp::load(Role::Peer, root.clone()).await.unwrap();
        assert_eq!(restarted.refresh_devices().await.unwrap().len(), 1);
        {
            let inner = restarted.inner.read().await;
            assert_eq!(inner.identity.node_id, first.node_id);
            assert!(inner.identity.pending_rebind_token.is_none());
            let snapshot = serde_json::to_string(&snapshot(&inner)).unwrap();
            assert!(!snapshot.contains(&inner.identity.machine_key));
            assert!(!snapshot.contains(&inner.identity.device_token));
        }
        restarted.shutdown().await;
        let recovered = ClientApp::load(Role::Peer, root).await.unwrap();
        assert!(recovered.refresh_devices().await.is_ok());
        fixture.cleanup(vec![original, recovered]).await;
    }

    #[tokio::test]
    async fn legacy_profile_without_login_can_visit_another_account_then_reclaim_original_device() {
        let fixture = Fixture::new().await;
        let original = fixture.client("legacy-origin").await;
        let first = sign_in(&original, "alice").await;
        let root = fixture.root.join("unclaimed-portable-profile");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let old_token = {
            let inner = original.inner.read().await;
            tokio::fs::write(root.join("identity.json"), serde_json::to_vec(&serde_json::json!({
                "node_id":inner.identity.node_id,"device_token":inner.identity.device_token
            })).unwrap()).await.unwrap();
            tokio::fs::copy(inner.root.join("machine.json"), root.join("machine.json")).await.unwrap();
            inner.identity.device_token.clone()
        };
        let legacy = fixture.client("unclaimed-portable-profile").await;
        assert!(!root.join("identity.json").exists());
        let bob = sign_in(&legacy, "bob").await;
        assert_ne!(bob.node_id, first.node_id);
        assert_ne!(legacy.inner.read().await.identity.device_token, old_token);
        let _ = logout(State(legacy.clone())).await.unwrap();
        let alice = sign_in(&legacy, "alice").await;
        assert_eq!(alice.node_id, first.node_id);
        assert_eq!(alice.devices.len(), 1);
        assert!(!alice.device_rebind_required);
        assert_eq!(legacy.inner.read().await.identity.device_token, old_token);
        fixture.cleanup(vec![original, legacy]).await;
    }

    #[tokio::test]
    async fn unchecked_remember_removes_saved_credentials() {
        let root=std::env::temp_dir().join(format!("starbridge-auth-{}",uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let auth=AuthResponse {access_token:"test-access".into(),username:"remembered-user".into(),
            expires_at:u64::MAX,refresh_token:Some("test-refresh".into()),refresh_expires_at:Some(u64::MAX)};
        save_auth(&root,&auth).await.unwrap();
        assert_eq!(load_auth(&root,true).await.unwrap().unwrap().username,"remembered-user");
        assert!(load_auth(&root,false).await.unwrap().is_none());
        assert!(!root.join("auth.secure").exists()); assert!(!root.join("auth.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
