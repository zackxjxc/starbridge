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
    let _gate = app.session_operations.lock().await;
    let (url, generation) = {
        let mut inner = app.inner.write().await;
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
    {
        let mut inner = app.inner.write().await;
        if generation != inner.auth_generation || app.shutting_down.load(Ordering::Acquire) {
            return Err(ApiError::bad_request("登录已取消"));
        }
        inner.auth = Some(auth.clone());
        inner.stage = "online".into();
        inner.last_error = None;
        if form.remember { save_auth(&inner.root, &auth).await.map_err(ApiError::internal)?; }
    }
    drop(_gate);
    if let Err(error) = app.refresh_devices().await {
        app.inner.write().await.last_error = Some(error.to_string());
    }
    Ok(Json(snapshot(&*app.inner.read().await)))
}

pub(super) async fn logout(State(app): State<ClientApp>) -> Result<Json<ClientSnapshot>, ApiError> {
    let _gate=app.session_operations.lock().await;
    {
        let mut inner=app.inner.write().await;
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
        inner.stage = "ready".into();
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

pub(super) async fn update_device(
    State(app): State<ClientApp>, Json(form): Json<DeviceMetadataForm>,
) -> Result<Json<ClientSnapshot>, ApiError> {
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
        let (auth, url, generation, registration) = {
            let inner = self.inner.read().await;
            if inner.closing_account { anyhow::bail!("正在退出登录"); }
            (inner.auth.clone().ok_or_else(|| anyhow::anyhow!("未登录"))?,
                inner.config.server_url.clone(), inner.auth_generation, device_registration(&inner))
        };
        let devices = self.http.post(format!("{url}/api/v1/devices/register"))
            .bearer_auth(auth.access_token).json(&registration).send().await?.error_for_status()?
            .json::<Vec<DeviceInfo>>().await?;
        if self.inner.read().await.auth_generation != generation { anyhow::bail!("登录状态已更改"); }
        self.sync_authorized_keys(&devices).await;
        let mut inner = self.inner.write().await;
        if inner.auth_generation != generation { anyhow::bail!("登录状态已更改"); }
        if let Some(local) = devices.iter().find(|device| device.node_id == inner.identity.node_id) {
            if inner.config.display_name != local.display_name || inner.config.purpose != local.purpose {
                inner.config.display_name = local.display_name.clone();
                inner.config.purpose = local.purpose.clone();
                save_json(&inner.root.join("config.json"), &inner.config).await?;
            }
        }
        inner.devices = devices.clone();
        Ok(devices)
    }
}

pub(super) async fn clear_saved_auth(root: &FsPath) {
    for name in ["auth.json", "auth.secure"] {
        let _ = tokio::fs::remove_file(root.join(name)).await;
    }
}

pub(super) async fn load_auth(root: &FsPath, remember: bool) -> anyhow::Result<Option<AuthResponse>> {
    if !remember { clear_saved_auth(root).await; return Ok(None); }
    let path = root.join(if cfg!(windows) { "auth.secure" } else { "auth.json" });
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => if cfg!(windows) { protect_credentials(&bytes, false)? } else { bytes },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Migrate a previous portable installation without saving a password.
            match tokio::fs::read(root.join("auth.json")).await {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) => return Err(error.into()),
    };
    let auth: Option<AuthResponse> = serde_json::from_slice(&bytes)?;
    if let Some(auth) = &auth { save_auth(root, auth).await?; }
    Ok(auth.filter(|auth| auth.expires_at > now_unix()
        || auth.refresh_expires_at.is_some_and(|expiry| expiry > now_unix())))
}

pub(super) async fn save_auth(root: &FsPath, auth: &AuthResponse) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(auth)?;
    let path = root.join(if cfg!(windows) { "auth.secure" } else { "auth.json" });
    let bytes = if cfg!(windows) { protect_credentials(&bytes, true)? } else { bytes };
    let pending = path.with_extension("pending");
    tokio::fs::write(&pending, bytes).await?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o600)).await?;
    }
    tokio::fs::rename(pending, &path).await?;
    if cfg!(windows) { let _ = tokio::fs::remove_file(root.join("auth.json")).await; }
    Ok(())
}

#[cfg(not(windows))]
fn protect_credentials(bytes: &[u8], _encrypt: bool) -> anyhow::Result<Vec<u8>> { Ok(bytes.to_vec()) }

#[cfg(windows)]
fn protect_credentials(bytes: &[u8], encrypt: bool) -> anyhow::Result<Vec<u8>> {
    #[repr(C)] struct Blob { size: u32, data: *mut u8 }
    #[link(name = "crypt32")]
    unsafe extern "system" {
        fn CryptProtectData(input: *const Blob, description: *const u16, entropy: *const Blob,
            reserved: *mut std::ffi::c_void, prompt: *mut std::ffi::c_void, flags: u32, output: *mut Blob) -> i32;
        fn CryptUnprotectData(input: *const Blob, description: *mut *mut u16, entropy: *const Blob,
            reserved: *mut std::ffi::c_void, prompt: *mut std::ffi::c_void, flags: u32, output: *mut Blob) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" { fn LocalFree(memory: *mut std::ffi::c_void) -> *mut std::ffi::c_void; }
    let input = Blob { size: u32::try_from(bytes.len())?, data: bytes.as_ptr() as *mut u8 };
    let mut output = Blob { size: 0, data: std::ptr::null_mut() };
    // DPAPI binds this credential to the current Windows user; no UI prompt is permitted.
    let result = unsafe { if encrypt {
        CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), 1, &mut output)
    } else {
        CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), 1, &mut output)
    }};
    if result == 0 { return Err(std::io::Error::last_os_error().into()); }
    let protected = unsafe { std::slice::from_raw_parts(output.data, output.size as usize).to_vec() };
    unsafe { LocalFree(output.data.cast()); }
    Ok(protected)
}

#[cfg(test)]
mod tests {
    use super::*;

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
