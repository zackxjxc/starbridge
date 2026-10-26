//! Machine identification is a lookup hint, separate from the device credential.
use crate::secure_store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Deserialize, Serialize)]
pub struct AccountBinding {
    pub node_id: String,
    pub device_token: String,
    #[serde(default)]
    pub pending_rebind_token: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Identity {
    pub node_id: String,
    pub device_token: String,
    #[serde(default)]
    pub machine_key: String,
    #[serde(default)]
    pub account_bindings: BTreeMap<String, AccountBinding>,
    #[serde(default)]
    pub active_account: Option<String>,
    #[serde(default)]
    pub pending_rebind_token: Option<String>,
    /// An old credential whose account cannot yet be established. Never discard on account switch.
    #[serde(default)]
    pub legacy_candidate: Option<AccountBinding>,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            node_id: starbridge_protocol::new_id(), device_token: starbridge_protocol::new_token(),
            machine_key: String::new(), account_bindings: BTreeMap::new(),
            active_account: None, pending_rebind_token: None, legacy_candidate: None,
        }
    }
}

impl Identity {
    /// Restore an already saved account session; retain the old credential until ownership is known.
    pub fn select_account(&mut self, scope: &str) {
        if self.active_account.as_deref() == Some(scope) { return; }
        let legacy = self.active_account.is_none() && self.account_bindings.is_empty();
        if legacy { self.retain_legacy_candidate(); }
        self.remember_account();
        if let Some(binding) = self.account_bindings.get(scope) {
            self.node_id = binding.node_id.clone();
            self.device_token = binding.device_token.clone();
            self.pending_rebind_token = binding.pending_rebind_token.clone();
        } else if !legacy {
            self.node_id = starbridge_protocol::new_id();
            self.device_token = starbridge_protocol::new_token();
            self.pending_rebind_token = None;
        }
        self.active_account = Some(scope.into());
        self.remember_account();
    }

    /// Choose credentials only after reading this authenticated account's actual device records.
    /// The machine hint or last entered username alone cannot establish legacy ownership.
    pub fn select_account_for_devices(&mut self, scope: &str, device_ids: &[String]) {
        if self.active_account.is_none() && self.account_bindings.is_empty() {
            self.retain_legacy_candidate();
        }
        self.remember_account();
        let binding = self.account_bindings.get(scope).cloned().or_else(|| {
            self.legacy_candidate.as_ref()
                .filter(|candidate| device_ids.contains(&candidate.node_id)).cloned()
        });
        if let Some(binding) = binding {
            if self.legacy_candidate.as_ref().is_some_and(|candidate|
                candidate.node_id == binding.node_id && device_ids.contains(&binding.node_id)) {
                self.legacy_candidate = None;
            }
            self.node_id = binding.node_id;
            self.device_token = binding.device_token;
            self.pending_rebind_token = binding.pending_rebind_token;
        } else {
            self.node_id = starbridge_protocol::new_id();
            self.device_token = starbridge_protocol::new_token();
            self.pending_rebind_token = None;
        }
        self.active_account = Some(scope.into());
        self.remember_account();
    }

    fn retain_legacy_candidate(&mut self) {
        if self.legacy_candidate.is_none() {
            self.legacy_candidate = Some(AccountBinding {
                node_id: self.node_id.clone(), device_token: self.device_token.clone(),
                pending_rebind_token: self.pending_rebind_token.clone(),
            });
        }
    }

    pub fn remember_account(&mut self) {
        if let Some(scope) = &self.active_account {
            self.account_bindings.insert(scope.clone(), AccountBinding {
                node_id: self.node_id.clone(), device_token: self.device_token.clone(),
                pending_rebind_token: self.pending_rebind_token.clone(),
            });
        }
    }
}

#[cfg(test)]
pub(super) async fn load_identity(root: &Path) -> anyhow::Result<Identity> {
    load_identity_with_interaction(root, secure_store::Interaction::Silent).await
}

pub(super) async fn load_identity_with_interaction(root: &Path, interaction: secure_store::Interaction) -> anyhow::Result<Identity> {
    let bytes = match secure_store::read_secret_with_interaction(root, "identity", interaction).await? {
        Some(bytes) => Some(bytes),
        None => match tokio::fs::read(root.join("identity.json")).await {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => anyhow::bail!("无法读取原设备身份，请检查数据目录权限"),
        },
    };
    // A corrupt or inaccessible credential must never cause regeneration.
    let mut identity: Identity = match bytes {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("原设备身份损坏，请保留数据目录并恢复备份"))?,
        None => Identity::default(),
    };
    if identity.node_id.is_empty() || identity.device_token.is_empty() {
        anyhow::bail!("原设备身份无效，请保留数据目录并恢复备份");
    }
    if identity.active_account.is_none() && identity.account_bindings.is_empty() {
        identity.retain_legacy_candidate();
    }
    let key = load_machine(root).await?;
    if !identity.machine_key.is_empty() && identity.machine_key != key {
        anyhow::bail!("设备识别来源发生变化；请保留原数据并通过账号验证处理设备绑定");
    }
    identity.machine_key = key;
    save_identity_with_interaction(root, &identity, interaction).await?;
    Ok(identity)
}

pub(super) async fn save_identity(root: &Path, identity: &Identity) -> anyhow::Result<()> {
    save_identity_with_interaction(root, identity, secure_store::Interaction::Silent).await
}

pub(super) async fn save_identity_with_interaction(root: &Path, identity: &Identity, interaction: secure_store::Interaction) -> anyhow::Result<()> {
    let mut identity = identity.clone();
    identity.remember_account();
    secure_store::write_secret_with_interaction(root, "identity", &serde_json::to_vec(&identity)?, interaction).await?;
    match tokio::fs::remove_file(root.join("identity.json")).await {
        Ok(()) => {},
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(_) => anyhow::bail!("已保存安全身份，但无法移除旧身份文件，请检查数据目录权限"),
    }
    Ok(())
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Source { NativeV1, FallbackV1, IsolatedV1 }

#[derive(Deserialize, Serialize)]
struct MachineState { version: u32, source: Source, key: String }

/// v1 contract: ASCII trim; optional single brace pair; UUID 8-4-4-4-12 or
/// 32 hex digits; lowercase 32 hex without separators; reject all-zero/all-FF.
/// Do not change this normalization after release; add a new explicit version.
fn normalize(raw: &str) -> Option<String> {
    let value = raw.trim_matches(|c: char| c.is_ascii_whitespace());
    let value = if value.starts_with('{') && value.ends_with('}') {
        &value[1..value.len() - 1]
    } else { value };
    if value.len() != 32 && value.len() != 36 { return None; }
    let mut normalized = String::with_capacity(32);
    for (index, byte) in value.bytes().enumerate() {
        if value.len() == 36 && [8, 13, 18, 23].contains(&index) {
            if byte != b'-' { return None; }
        } else {
            if !byte.is_ascii_hexdigit() { return None; }
            normalized.push((byte as char).to_ascii_lowercase());
        }
    }
    if normalized.bytes().all(|b| b == b'0') || normalized.bytes().all(|b| b == b'f') { return None; }
    Some(normalized)
}

fn derive(namespace: &[u8], value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace);
    digest.update(value.as_bytes());
    format!("{:x}", digest.finalize())
}

async fn load_machine(root: &Path) -> anyhow::Result<String> {
    let native = || machine_uid::get().ok().and_then(|value| normalize(&value));
    load_machine_with(root, crate::data_directory::is_default(root), native).await
}

async fn load_machine_with(root: &Path, default_profile: bool, read_native: impl FnOnce() -> Option<String>) -> anyhow::Result<String> {
    let path = root.join("machine.json");
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let state: MachineState = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("设备识别状态损坏，请保留数据目录并恢复备份"))?;
            if state.version != 1 || state.key.len() != 64
                || !state.key.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) {
                anyhow::bail!("设备识别状态无效，请保留数据目录并恢复备份");
            }
            if !default_profile && state.source != Source::IsolatedV1 {
                anyhow::bail!("此目录含默认设备身份，不能作为独立测试实例使用。请为 --data-dir 选择空目录；保留正式身份请使用 --import-data-dir 导入默认用户目录");
            }
            if state.source == Source::NativeV1 {
                if let Some(value) = read_native() {
                    if derive(b"starbridge:machine:v1\0", &value) != state.key {
                        anyhow::bail!("系统设备标识已改变，请保留原数据并通过账号验证处理设备绑定");
                    }
                }
            }
            // A transient read failure or recovered native source never changes a pinned identity.
            return Ok(state.key);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(_) => anyhow::bail!("无法读取设备识别状态，请检查数据目录权限"),
    }
    let (source, key) = if !default_profile {
        (Source::IsolatedV1, derive(b"starbridge:instance:v1\0", &uuid::Uuid::new_v4().simple().to_string()))
    } else if let Some(value) = read_native() {
        (Source::NativeV1, derive(b"starbridge:machine:v1\0", &value))
    } else {
        (Source::FallbackV1, derive(b"starbridge:fallback:v1\0", &uuid::Uuid::new_v4().simple().to_string()))
    };
    let state = MachineState { version: 1, source, key };
    secure_store::write_private(&path, &serde_json::to_vec(&state)?).await?;
    Ok(state.key)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("starbridge-identity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }
    #[test]
    fn normalization_v1_is_stable_and_rejects_invalid_values() {
        let expected = "00112233445566778899aabbccddeeff";
        for value in ["00112233445566778899aabbccddeeff", "00112233-4455-6677-8899-AABBCCDDEEFF", " \t{00112233-4455-6677-8899-AABBCCDDEEFF}\r\n"] {
            assert_eq!(normalize(value).as_deref(), Some(expected));
        }
        for value in ["", "00000000-0000-0000-0000-000000000000", "ffffffffffffffffffffffffffffffff", "00112233 4455 6677 8899 AABBCCDDEEFF", "{00112233445566778899aabbccddeeff", "00112233445566778899aabbccddeefg", "\u{00a0}00112233445566778899aabbccddeeff"] {
            assert!(normalize(value).is_none());
        }
        assert_eq!(derive(b"starbridge:machine:v1\0", expected), "21c9ff371964d117f71873bbc1f933fa347d7d3c2e858199418daee93dd1b530");
    }
    #[tokio::test]
    async fn native_pins_across_restart_and_transient_failure() {
        let root = root();
        let native = || normalize("00112233-4455-6677-8899-AABBCCDDEEFF");
        let first = load_machine_with(&root, true, native).await.unwrap();
        assert_eq!(load_machine_with(&root, true, || None).await.unwrap(), first);
        assert_eq!(load_machine_with(&root, true, native).await.unwrap(), first);
        assert!(load_machine_with(&root, true, || normalize("11112233-4455-6677-8899-aabbccddeeff")).await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn fallback_is_persistent_even_after_native_recovers() {
        let root = root();
        let first = load_machine_with(&root, true, || None).await.unwrap();
        assert_eq!(load_machine_with(&root, true, || None).await.unwrap(), first);
        assert_eq!(load_machine_with(&root, true, || normalize("00112233445566778899aabbccddeeff")).await.unwrap(), first);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn independent_profiles_do_not_share_machine_keys() {
        let one = root(); let two = root(); let formal = root();
        let native = || normalize("00112233445566778899aabbccddeeff");
        let a = load_machine_with(&one, false, native).await.unwrap();
        let b = load_machine_with(&two, false, native).await.unwrap();
        let production = load_machine_with(&formal, true, native).await.unwrap();
        assert_ne!(a, b); assert_ne!(a, production); assert_ne!(b, production);
        assert_eq!(load_machine_with(&one, false, native).await.unwrap(), a);
        for root in [one, two, formal] { std::fs::remove_dir_all(root).unwrap(); }
    }
    #[tokio::test]
    async fn a_formal_profile_copy_cannot_be_used_as_a_test_instance() {
        let original = root(); let copied = root();
        load_machine_with(&original, true, || normalize("00112233445566778899aabbccddeeff")).await.unwrap();
        std::fs::copy(original.join("machine.json"), copied.join("machine.json")).unwrap();
        assert!(load_machine_with(&copied, false, || normalize("00112233445566778899aabbccddeeff")).await.is_err());
        for root in [original, copied] { std::fs::remove_dir_all(root).unwrap(); }
    }
    #[tokio::test]
    async fn legacy_identity_and_account_bindings_survive_secure_migration() {
        let root = root();
        std::fs::write(root.join("identity.json"), br#"{"node_id":"legacy-node","device_token":"legacy-token"}"#).unwrap();
        let mut identity = load_identity(&root).await.unwrap();
        assert_eq!(identity.node_id, "legacy-node"); assert_eq!(identity.device_token, "legacy-token");
        assert!(!root.join("identity.json").exists());
        identity.select_account("server:a");
        identity.select_account("server:b");
        let other = identity.device_token.clone();
        assert_ne!(other, "legacy-token");
        identity.select_account("server:a");
        assert_eq!(identity.device_token, "legacy-token");
        save_identity(&root, &identity).await.unwrap();
        let mut restored = load_identity(&root).await.unwrap();
        restored.select_account("server:b"); assert_eq!(restored.device_token, other);
        secure_store::remove_secret(&root, "identity").await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn unknown_legacy_account_survives_another_account_login_and_restart() {
        let root = root();
        std::fs::write(root.join("identity.json"), br#"{"node_id":"legacy-node","device_token":"legacy-token"}"#).unwrap();
        let mut identity = load_identity(&root).await.unwrap();
        identity.select_account_for_devices("server:other", &[]);
        assert_ne!(identity.node_id, "legacy-node");
        assert_ne!(identity.device_token, "legacy-token");
        let other_node = identity.node_id.clone(); let other_token = identity.device_token.clone();
        assert_eq!(identity.legacy_candidate.as_ref().unwrap().device_token, "legacy-token");
        save_identity(&root, &identity).await.unwrap();
        let mut restored = load_identity(&root).await.unwrap();
        restored.select_account_for_devices("server:original", &["legacy-node".into()]);
        assert_eq!(restored.node_id, "legacy-node"); assert_eq!(restored.device_token, "legacy-token");
        assert!(restored.legacy_candidate.is_none());
        restored.select_account_for_devices("server:other", &[other_node.clone()]);
        assert_eq!(restored.node_id, other_node); assert_eq!(restored.device_token, other_token);
        save_identity(&root, &restored).await.unwrap();
        let mut restarted = load_identity(&root).await.unwrap();
        restarted.select_account_for_devices("server:original", &["legacy-node".into()]);
        assert_eq!(restarted.device_token, "legacy-token");
        secure_store::remove_secret(&root, "identity").await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn native_machine_uid_is_accepted_without_exposing_it() {
        #[cfg(any(windows, target_os = "macos"))]
        assert!(machine_uid::get().ok().and_then(|value| normalize(&value)).is_some(), "系统机器标识读取或格式验证失败");
    }
}
