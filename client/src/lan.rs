use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    thread::JoinHandle,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const SERVICE_TYPE: &str = "_starbridge._tcp.local.";
const LAN_PROTOCOL_VERSION: &str = "1";
const PRESENCE_TIMEOUT_SECS: u64 = 15;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LanProfile {
    pub group_id: String,
    pub group_name: String,
    pub invite_secret: String,
    pub node_id: String,
    pub nickname: String,
}

impl LanProfile {
    pub(crate) fn new(group_name: String, node_id: String, nickname: String) -> Self {
        Self {
            group_id: Uuid::new_v4().simple().to_string(),
            group_name,
            invite_secret: Uuid::new_v4().simple().to_string(),
            node_id,
            nickname,
        }
    }

    pub(crate) fn invite_code(&self) -> String {
        format!(
            "SB1-{}-{}",
            self.group_id.to_uppercase(),
            self.invite_secret.to_uppercase()
        )
    }

    pub(crate) fn proof_for(&self, node_id: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.invite_secret.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.group_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(node_id.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    pub(crate) fn parse_invite(code: &str) -> anyhow::Result<(String, String)> {
        let compact = code
            .trim()
            .replace(['-', ' ', '\n', '\r', '\t'], "")
            .to_uppercase();
        if !compact.starts_with("SB1") || compact.len() != 67 {
            anyhow::bail!("邀请码格式无效");
        }
        let group_id = compact[3..35].to_lowercase();
        let secret = compact[35..].to_lowercase();
        if !group_id.chars().all(|c| c.is_ascii_hexdigit())
            || !secret.chars().all(|c| c.is_ascii_hexdigit())
        {
            anyhow::bail!("邀请码格式无效");
        }
        Ok((group_id, secret))
    }

    pub(crate) fn from_invite(
        code: &str,
        node_id: String,
        nickname: String,
    ) -> anyhow::Result<Self> {
        let (group_id, invite_secret) = Self::parse_invite(code)?;
        Ok(Self {
            group_id,
            group_name: String::new(),
            invite_secret,
            node_id,
            nickname,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LanPeer {
    pub node_id: String,
    pub nickname: String,
    pub group_name: String,
    pub address: String,
    pub port: u16,
    pub online: bool,
    pub last_seen: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct LanSnapshot {
    pub active: bool,
    pub group_id: Option<String>,
    pub group_name: Option<String>,
    pub invite_code: Option<String>,
    pub node_id: Option<String>,
    pub nickname: Option<String>,
    pub peers: Vec<LanPeer>,
}

#[derive(Clone)]
struct PeerRecord {
    peer: LanPeer,
    service_name: String,
}

pub(crate) struct LanRuntime {
    profile: RwLock<Option<LanProfile>>,
    peers: Arc<RwLock<HashMap<String, PeerRecord>>>,
    daemon: Mutex<Option<ServiceDaemon>>,
    listener: Mutex<Option<JoinHandle<()>>>,
}

impl LanRuntime {
    pub(crate) fn new() -> Self {
        Self {
            profile: RwLock::new(None),
            peers: Arc::new(RwLock::new(HashMap::new())),
            daemon: Mutex::new(None),
            listener: Mutex::new(None),
        }
    }

    pub(crate) fn profile(&self) -> Option<LanProfile> {
        self.profile.read().ok().and_then(|profile| profile.clone())
    }

    pub(crate) fn snapshot(&self) -> LanSnapshot {
        let profile = self.profile();
        let now = unix_now();
        let mut peers = self
            .peers
            .write()
            .unwrap_or_else(|error| error.into_inner());
        peers
            .retain(|_, record| now.saturating_sub(record.peer.last_seen) <= PRESENCE_TIMEOUT_SECS);
        let mut items: Vec<_> = peers.values().map(|record| record.peer.clone()).collect();
        items.sort_by(|a, b| {
            a.nickname
                .cmp(&b.nickname)
                .then_with(|| a.node_id.cmp(&b.node_id))
        });
        LanSnapshot {
            active: profile.is_some(),
            group_id: profile.as_ref().map(|value| value.group_id.clone()),
            group_name: profile
                .as_ref()
                .map(|value| value.group_name.clone())
                .filter(|value| !value.is_empty()),
            invite_code: profile.as_ref().map(LanProfile::invite_code),
            node_id: profile.as_ref().map(|value| value.node_id.clone()),
            nickname: profile.as_ref().map(|value| value.nickname.clone()),
            peers: items,
        }
    }

    pub(crate) fn start(&self, profile: LanProfile) -> anyhow::Result<()> {
        self.stop_discovery();
        let daemon =
            ServiceDaemon::new().map_err(|error| anyhow::anyhow!("无法启动局域网发现：{error}"))?;
        let instance = format!("starbridge-{}", profile.node_id.replace('-', ""));
        let host = format!("starbridge-{}.local.", profile.node_id.replace('-', ""));
        let mut properties = HashMap::new();
        properties.insert("v".to_string(), LAN_PROTOCOL_VERSION.to_string());
        properties.insert("group".to_string(), profile.group_id.clone());
        properties.insert("node".to_string(), profile.node_id.clone());
        properties.insert("name".to_string(), profile.nickname.clone());
        properties.insert("group_name".to_string(), profile.group_name.clone());
        properties.insert("proof".to_string(), profile.proof_for(&profile.node_id));
        let info = ServiceInfo::new(SERVICE_TYPE, &instance, &host, "", 0, Some(properties))
            .map_err(|error| anyhow::anyhow!("无法发布局域网设备：{error}"))?;
        daemon
            .register(info)
            .map_err(|error| anyhow::anyhow!("无法发布局域网设备：{error}"))?;
        let receiver = daemon
            .browse(SERVICE_TYPE)
            .map_err(|error| anyhow::anyhow!("无法监听局域网设备：{error}"))?;
        let peers = Arc::clone(&self.peers);
        let own_node_id = profile.node_id.clone();
        let own_group_id = profile.group_id.clone();
        let invite_secret = profile.invite_secret.clone();
        let listener = std::thread::spawn(move || {
            while let Ok(event) = receiver.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let Some(group_id) = info.get_property_val_str("group") else {
                            continue;
                        };
                        let Some(node_id) = info.get_property_val_str("node") else {
                            continue;
                        };
                        let Some(proof) = info.get_property_val_str("proof") else {
                            continue;
                        };
                        if group_id != own_group_id || node_id == own_node_id || proof.is_empty() {
                            continue;
                        }
                        let expected = proof_for(&invite_secret, group_id, node_id);
                        if proof != expected {
                            continue;
                        }
                        let address = info
                            .get_addresses_v4()
                            .into_iter()
                            .next()
                            .map(|ip| ip.to_string())
                            .or_else(|| {
                                info.get_addresses().iter().next().map(|ip| ip.to_string())
                            });
                        let Some(address) = address else {
                            continue;
                        };
                        let peer = LanPeer {
                            node_id: node_id.to_string(),
                            nickname: info
                                .get_property_val_str("name")
                                .unwrap_or(node_id)
                                .to_string(),
                            group_name: info
                                .get_property_val_str("group_name")
                                .unwrap_or_default()
                                .to_string(),
                            address,
                            port: info.get_port(),
                            online: true,
                            last_seen: unix_now(),
                        };
                        peers
                            .write()
                            .unwrap_or_else(|error| error.into_inner())
                            .insert(
                                node_id.to_string(),
                                PeerRecord {
                                    peer,
                                    service_name: info.get_fullname().to_string(),
                                },
                            );
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        peers
                            .write()
                            .unwrap_or_else(|error| error.into_inner())
                            .retain(|_, record| record.service_name != fullname);
                    }
                    _ => {}
                }
            }
        });
        *self
            .profile
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(profile);
        *self
            .daemon
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(daemon);
        *self
            .listener
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(listener);
        Ok(())
    }

    pub(crate) fn stop(&self) {
        self.stop_discovery();
        *self
            .profile
            .write()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }

    pub(crate) fn stop_discovery(&self) {
        if let Some(daemon) = self
            .daemon
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = daemon.shutdown();
        }
        if let Some(listener) = self
            .listener
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = listener.join();
        }
        self.peers
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    pub(crate) fn replace_profile(&self, profile: LanProfile) -> anyhow::Result<()> {
        self.start(profile)
    }

    pub(crate) fn resume(&self) -> anyhow::Result<()> {
        let Some(profile) = self.profile() else { return Ok(()); };
        self.start(profile)
    }
}

fn proof_for(invite_secret: &str, group_id: &str, node_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(invite_secret.as_bytes());
    hasher.update(b"\0");
    hasher.update(group_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(node_id.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permanent_invite_round_trips_without_exposing_group_secret_in_discovery() {
        let profile = LanProfile::new("星桥实验组".into(), "node-a".into(), "办公本".into());
        let code = profile.invite_code();
        let joined = LanProfile::from_invite(&code, "node-b".into(), "测试机".into()).unwrap();
        assert_eq!(joined.group_id, profile.group_id);
        assert_eq!(joined.invite_secret, profile.invite_secret);
        assert_ne!(profile.proof_for("node-a"), profile.invite_secret);
    }

    #[test]
    fn malformed_invite_is_rejected() {
        assert!(LanProfile::from_invite("SB1-NOT-A-CODE", "node".into(), "name".into()).is_err());
    }
}
