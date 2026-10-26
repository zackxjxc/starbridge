//! Keep a failed profile outside ClientApp so no command can replace its identity.
use crate::{ClientApp, secure_store::Interaction};
use crate::profile::{FailureKind, ProfileBackup, StartupIssue};
use starbridge_protocol::Role;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

pub(crate) struct ProfileStartup {
    role: Role,
    root: PathBuf,
    state: Mutex<StartupState>,
    closing: AtomicBool,
}

struct StartupState { core: Option<ClientApp>, error: Option<StartupIssue> }

impl ProfileStartup {
    pub(crate) fn new(role: Role, root: PathBuf, loaded: anyhow::Result<ClientApp>) -> Self {
        let (core, error) = match loaded {
            Ok(core) => (Some(core), None),
            Err(error) => (None, Some(StartupIssue::from_error(&error))),
        };
        Self { role, root, state: Mutex::new(StartupState { core, error }), closing: AtomicBool::new(false) }
    }

    pub(crate) async fn status(&self) -> Option<StartupIssue> { self.state.lock().await.error.clone() }

    #[cfg(test)]
    pub(crate) async fn error(&self) -> Option<String> { self.status().await.map(|issue| issue.message) }

    pub(crate) async fn retry(&self) -> Result<ClientApp, String> {
        self.retry_with(|| ClientApp::load_with_interaction(self.role, self.root.clone(), Interaction::UserInitiated)).await
    }

    async fn retry_with<F: std::future::Future<Output = anyhow::Result<ClientApp>>>(
        &self, load: impl FnOnce() -> F,
    ) -> Result<ClientApp, String> {
        // One recovery at a time; no polling or other command can prompt for access.
        let mut state = self.state.lock().await;
        if self.closing.load(Ordering::Acquire) { return Err("客户端正在退出".into()); }
        if let Some(core) = &state.core { return Ok(core.clone()); }
        match load().await {
            Ok(core) => {
                if self.closing.load(Ordering::Acquire) {
                    core.shutdown().await;
                    return Err("客户端正在退出".into());
                }
                state.core = Some(core.clone()); state.error = None; Ok(core)
            }
            Err(error) => {
                state.error = Some(StartupIssue::from_error(&error)); Err(error.to_string())
            }
        }
    }

    pub(crate) fn begin_shutdown(&self) { self.closing.store(true, Ordering::Release); }

    pub(crate) async fn recover_account(&self, form: crate::LoginForm) -> Result<ClientApp, String> {
        if form.username.trim().is_empty() || form.password.len() < 4 { return Err("请输入账户名称和至少 4 位密码".into()); }
        let mut state = self.state.lock().await;
        if self.closing.load(Ordering::Acquire) { return Err("客户端正在退出".into()); }
        if state.core.is_some() || !state.error.as_ref().is_some_and(|issue| issue.account_recovery) {
            return Err("当前不需要通过账号恢复设备".into());
        }
        let config = crate::load_json(&self.root.join("config.json"), crate::ClientConfig::default()).await
            .map_err(|_| "本机配置无法读取，请先检查配置后重试".to_string())?;
        let http = reqwest::Client::builder().no_proxy().timeout(std::time::Duration::from_secs(15))
            .build().map_err(|_| "无法连接账号服务，请稍后重试".to_string())?;
        let response = http.post(format!("{}/api/v1/auth/login", config.server_url))
            .json(&starbridge_protocol::AuthRequest { username: form.username.trim().into(), password: form.password.clone() })
            .send().await.map_err(|_| "无法连接账号服务，请稍后重试".to_string())?;
        let auth: starbridge_protocol::AuthResponse = crate::account::response_json(response).await.map_err(|error| error.message)?;
        if auth.username != form.username.trim() || auth.access_token.is_empty() {
            return Err("账号验证结果不匹配，请重新登录".into());
        }
        // No profile changes occur until the password has been verified by the configured server.
        let reset_machine = state.error.as_ref().is_some_and(|issue| issue.kind == FailureKind::Machine)
            || crate::identity::recovery_requires_machine_reset(&self.root).await;
        let result = self.recover_verified(&mut state, form, auth.clone(), reset_machine).await;
        if result.is_err() {
            let _ = http.post(format!("{}/api/v1/auth/logout", config.server_url)).bearer_auth(auth.access_token)
                .json(&serde_json::json!({"refresh_token":auth.refresh_token})).send().await;
        }
        result
    }

    async fn recover_verified(&self, state: &mut StartupState, form: crate::LoginForm,
        auth: starbridge_protocol::AuthResponse, reset_machine: bool) -> Result<ClientApp, String>
    {
        if self.closing.load(Ordering::Acquire) { return Err("客户端正在退出".into()); }
        let mut names = vec!["identity.secure", "identity.json", "auth.secure", "auth.json"];
        if reset_machine { names.push("machine.json"); }
        let backup = ProfileBackup::create(&self.root, &names, true).await
            .map_err(|_| "无法备份原设备资料，请检查数据目录权限".to_string())?;
        let core = match ClientApp::load(self.role, self.root.clone()).await {
            Ok(core) => core,
            Err(error) => {
                backup.rollback_recovery(reset_machine).await.map_err(|_| "恢复未完成，原资料备份已保留，请检查数据目录权限".to_string())?;
                state.error = Some(StartupIssue::from_error(&error));
                return Err(error.to_string());
            }
        };
        if self.closing.load(Ordering::Acquire) {
            core.shutdown().await;
            backup.rollback_recovery(reset_machine).await.map_err(|_| "恢复未完成，原资料备份已保留，请检查数据目录权限".to_string())?;
            return Err("客户端正在退出".into());
        }
        if let Err(error) = crate::account::recover_verified_account(&core, form, auth).await {
            core.shutdown().await;
            backup.rollback_recovery(reset_machine).await.map_err(|_| "恢复未完成，原资料备份已保留，请检查数据目录权限".to_string())?;
            return Err(error.message);
        }
        // Enrollment can have committed remotely: keep the new pending credential even
        // if the user closes now, so a later startup can finish without reviving old data.
        state.core = Some(core.clone()); state.error = None;
        if self.closing.load(Ordering::Acquire) { core.shutdown().await; }
        Ok(core)
    }

    pub(crate) async fn shutdown(&self) {
        let core = self.state.lock().await.core.clone();
        if let Some(core) = core { core.shutdown().await; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecoveryFixture {
        root: PathBuf,
        profile: PathBuf,
        core: ClientApp,
        server: tokio::task::JoinHandle<()>,
    }

    impl RecoveryFixture {
        async fn new() -> Self {
            let root = std::env::temp_dir().join(format!("starbridge-account-recovery-{}", uuid::Uuid::new_v4()));
            let profile = root.join("profile");
            tokio::fs::create_dir_all(&profile).await.unwrap();
            let state = starbridge_server::AppState::with_frp_and_data("127.0.0.1".into(), 17851, "test".into(), root.join("server"));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, starbridge_server::app(state)).await.unwrap(); });
            reqwest::Client::new().post(format!("{url}/api/v1/auth/register"))
                .json(&starbridge_protocol::AuthRequest { username: "recovery-user".into(), password: "recovery-password".into() })
                .send().await.unwrap().error_for_status().unwrap();
            crate::save_json(&profile.join("config.json"), &crate::ClientConfig { server_url: url, ..Default::default() }).await.unwrap();
            let core = ClientApp::load(Role::Peer, profile.clone()).await.unwrap();
            let _ = crate::account::login(axum::extract::State(core.clone()), axum::Json(Self::form("recovery-password"))).await.unwrap();
            core.shutdown().await;
            Self { root, profile, core, server }
        }

        fn form(password: &str) -> crate::LoginForm {
            crate::LoginForm { username: "recovery-user".into(), password: password.into(), remember: true }
        }

        async fn corrupt_identity(&self) -> ProfileStartup {
            crate::secure_store::write_secret(&self.profile, "identity", b"unreadable-original-identity").await.unwrap();
            ProfileStartup::new(Role::Peer, self.profile.clone(), ClientApp::load(Role::Peer, self.profile.clone()).await)
        }

        async fn finish(self, startup: &ProfileStartup) {
            startup.begin_shutdown(); startup.shutdown().await;
            self.server.abort(); let _ = self.server.await;
            for name in ["identity", "auth"] { crate::secure_store::remove_secret(&self.profile, name).await.unwrap(); }
            if let Ok(mut entries) = tokio::fs::read_dir(self.profile.join("recovery-backups")).await {
                while let Some(entry) = entries.next_entry().await.unwrap() {
                    for name in ["identity", "auth"] { crate::secure_store::remove_secret(&entry.path(), name).await.unwrap(); }
                }
            }
            std::fs::remove_dir_all(self.root).unwrap();
        }
    }

    #[tokio::test]
    async fn wrong_password_preserves_files_then_verified_recovery_reuses_the_original_record() {
        let fixture = RecoveryFixture::new().await;
        let original_node = fixture.core.inner.read().await.identity.node_id.clone();
        let original_token = fixture.core.inner.read().await.identity.device_token.clone();
        let startup = fixture.corrupt_identity().await;
        assert!(startup.status().await.unwrap().account_recovery);
        let original = tokio::fs::read(fixture.profile.join("identity.secure")).await.unwrap();
        let machine = tokio::fs::read(fixture.profile.join("machine.json")).await.unwrap();
        assert!(startup.recover_account(RecoveryFixture::form("incorrect-password")).await.is_err());
        assert_eq!(tokio::fs::read(fixture.profile.join("identity.secure")).await.unwrap(), original);
        assert!(!fixture.profile.join("recovery-backups").exists());
        let recovered = startup.recover_account(RecoveryFixture::form("recovery-password")).await.unwrap();
        assert!(startup.status().await.is_none());
        let inner = recovered.inner.read().await;
        assert_eq!(inner.identity.node_id, original_node);
        assert_ne!(inner.identity.device_token, original_token);
        assert!(!inner.device_rebind_required);
        assert_eq!(inner.devices.len(), 1);
        drop(inner);
        assert_eq!(tokio::fs::read(fixture.profile.join("machine.json")).await.unwrap(), machine);
        let backup = std::fs::read_dir(fixture.profile.join("recovery-backups")).unwrap().next().unwrap().unwrap().path();
        assert_eq!(std::fs::read(backup.join("identity.secure")).unwrap(), original);
        fixture.finish(&startup).await;
    }

    #[tokio::test]
    async fn combined_identity_and_machine_recovery_needs_one_password_and_keeps_the_old_record() {
        let fixture = RecoveryFixture::new().await;
        let old_node = fixture.core.inner.read().await.identity.node_id.clone();
        tokio::fs::write(fixture.profile.join("machine.json"), b"broken-machine-state").await.unwrap();
        let startup = fixture.corrupt_identity().await;
        assert_eq!(startup.status().await.unwrap().kind, FailureKind::Identity);
        let recovered = startup.recover_account(RecoveryFixture::form("recovery-password")).await.unwrap();
        let inner = recovered.inner.read().await;
        assert_ne!(inner.identity.node_id, old_node);
        assert!(inner.devices.iter().any(|device| device.node_id == old_node));
        assert_eq!(inner.devices.len(), 2);
        drop(inner); fixture.finish(&startup).await;
    }

    #[tokio::test]
    async fn recovery_failure_restores_original_files_and_reports_the_new_failure_kind() {
        let fixture = RecoveryFixture::new().await;
        let startup = fixture.corrupt_identity().await;
        let original = tokio::fs::read(fixture.profile.join("identity.secure")).await.unwrap();
        let auth = tokio::fs::read(fixture.profile.join("auth.secure")).await.unwrap();
        tokio::fs::write(fixture.profile.join("last-test.json"), b"broken-test-state").await.unwrap();
        assert!(startup.recover_account(RecoveryFixture::form("recovery-password")).await.is_err());
        assert_eq!(tokio::fs::read(fixture.profile.join("identity.secure")).await.unwrap(), original);
        assert_eq!(tokio::fs::read(fixture.profile.join("auth.secure")).await.unwrap(), auth);
        assert_eq!(startup.status().await.unwrap().kind, FailureKind::Storage);
        assert!(startup.state.lock().await.core.is_none());
        fixture.finish(&startup).await;
    }

    #[tokio::test]
    async fn authorization_and_shutdown_never_allow_an_account_reset() {
        let root = std::env::temp_dir().join(format!("starbridge-recovery-denied-{}", uuid::Uuid::new_v4()));
        let startup = ProfileStartup::new(Role::Peer, root.clone(), Err(crate::profile::failure(FailureKind::Authorization, "authorization required")));
        assert!(!startup.status().await.unwrap().account_recovery);
        assert!(startup.recover_account(RecoveryFixture::form("recovery-password")).await.is_err());
        startup.begin_shutdown();
        assert!(startup.recover_account(RecoveryFixture::form("recovery-password")).await.is_err());
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn failed_recovery_preserves_profile_and_can_recover_after_access_is_restored() {
        let root = std::env::temp_dir().join(format!("starbridge-startup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("identity.json");
        let original = b"unreadable-original-identity";
        std::fs::write(&path, original).unwrap();
        let startup = ProfileStartup::new(Role::Peer, root.clone(), ClientApp::load(Role::Peer, root.clone()).await);
        assert!(startup.error().await.is_some());
        assert!(startup.retry().await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(!root.join("identity.secure").exists());
        assert!(!root.join("ssh").exists());
        let identity = crate::Identity::default();
        std::fs::write(&path, serde_json::to_vec(&identity).unwrap()).unwrap();
        let core = startup.retry().await.unwrap();
        assert!(startup.error().await.is_none());
        assert_eq!(core.inner.read().await.identity.node_id, identity.node_id);
        core.shutdown().await;
        crate::secure_store::remove_secret(&root, "identity").await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn corrupt_login_cache_is_preserved_and_does_not_block_password_login() {
        let root = std::env::temp_dir().join(format!("starbridge-startup-auth-{}", uuid::Uuid::new_v4()));
        let core = ClientApp::load(Role::Peer, root.clone()).await.unwrap();
        let node_id = core.inner.read().await.identity.node_id.clone();
        core.shutdown().await;
        let auth = root.join("auth.json");
        let original = b"unreadable-original-login";
        std::fs::write(&auth, original).unwrap();
        let recovered = ClientApp::load(Role::Peer, root.clone()).await.unwrap();
        assert!(recovered.inner.read().await.auth.is_none());
        assert_eq!(recovered.inner.read().await.identity.node_id, node_id);
        assert!(recovered.inner.read().await.last_error.as_ref().unwrap().contains("登录缓存"));
        assert!(!auth.exists());
        let backup = std::fs::read_dir(root.join("recovery-backups")).unwrap().next().unwrap().unwrap().path();
        assert_eq!(std::fs::read(backup.join("auth.json")).unwrap(), original);
        assert!(!root.join("ssh").exists());
        recovered.shutdown().await;
        crate::secure_store::remove_secret(&root, "identity").await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn closing_recovery_cannot_create_a_profile_or_start_authorization() {
        let root = std::env::temp_dir().join(format!("starbridge-startup-close-{}", uuid::Uuid::new_v4()));
        let startup = ProfileStartup::new(Role::Peer, root.clone(), Err(anyhow::anyhow!("access denied")));
        startup.begin_shutdown();
        assert!(startup.retry().await.is_err());
        startup.shutdown().await;
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn denying_authorization_keeps_recovery_available_without_creating_a_core() {
        let root = std::env::temp_dir().join(format!("starbridge-startup-denied-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let reference = root.join("identity.secure");
        std::fs::write(&reference, b"original-keychain-reference").unwrap();
        let startup = ProfileStartup::new(Role::Peer, root.clone(), Err(anyhow::anyhow!("access required")));
        for _ in 0..2 {
            let denied = startup.retry_with(|| async { anyhow::bail!("authorization denied") }).await;
            assert!(denied.is_err());
            assert_eq!(startup.error().await.as_deref(), Some("authorization denied"));
            assert!(startup.state.lock().await.core.is_none());
            assert_eq!(std::fs::read(&reference).unwrap(), b"original-keychain-reference");
            assert!(!root.join("machine.json").exists());
            assert!(!root.join("ssh").exists());
        }
        startup.begin_shutdown(); startup.shutdown().await;
        std::fs::remove_dir_all(root).unwrap();
    }
}
