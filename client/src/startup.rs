//! Keep a failed profile outside ClientApp so no command can replace its identity.
use crate::{ClientApp, secure_store::Interaction};
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

struct StartupState { core: Option<ClientApp>, error: Option<String> }

impl ProfileStartup {
    pub(crate) fn new(role: Role, root: PathBuf, loaded: anyhow::Result<ClientApp>) -> Self {
        let (core, error) = match loaded {
            Ok(core) => (Some(core), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self { role, root, state: Mutex::new(StartupState { core, error }), closing: AtomicBool::new(false) }
    }

    pub(crate) async fn error(&self) -> Option<String> { self.state.lock().await.error.clone() }

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
                let error = error.to_string(); state.error = Some(error.clone()); Err(error)
            }
        }
    }

    pub(crate) fn begin_shutdown(&self) { self.closing.store(true, Ordering::Release); }

    pub(crate) async fn shutdown(&self) {
        let core = self.state.lock().await.core.clone();
        if let Some(core) = core { core.shutdown().await; }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn unreadable_login_data_enters_recovery_without_replacing_device_identity() {
        let root = std::env::temp_dir().join(format!("starbridge-startup-auth-{}", uuid::Uuid::new_v4()));
        let core = ClientApp::load(Role::Peer, root.clone()).await.unwrap();
        let node_id = core.inner.read().await.identity.node_id.clone();
        core.shutdown().await;
        let auth = root.join("auth.json");
        let original = b"unreadable-original-login";
        std::fs::write(&auth, original).unwrap();
        let startup = ProfileStartup::new(Role::Peer, root.clone(), ClientApp::load(Role::Peer, root.clone()).await);
        assert!(startup.error().await.is_some());
        assert!(startup.retry().await.is_err());
        assert_eq!(std::fs::read(&auth).unwrap(), original);
        assert!(!root.join("ssh").exists());
        std::fs::write(&auth, b"null").unwrap();
        let recovered = startup.retry().await.unwrap();
        assert_eq!(recovered.inner.read().await.identity.node_id, node_id);
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
