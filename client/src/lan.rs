use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, SwarmBuilder, identify, identity,
    mdns::{self, Event as MdnsEvent},
    noise, ping,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;
use uuid::Uuid;

const LAN_PROTOCOL_VERSION: &str = "/starbridge/lan/1";
const PRESENCE_TIMEOUT_SECS: u64 = 15;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LanProfile {
    pub group_id: String,
    pub group_name: String,
    pub invite_secret: String,
    pub node_id: String,
    pub nickname: String,
    /// A persistent libp2p identity. It is kept inside the existing secure store.
    #[serde(default)]
    pub control_key: Vec<u8>,
}

impl LanProfile {
    pub(crate) fn new(group_name: String, node_id: String, nickname: String) -> Self {
        Self {
            group_id: Uuid::new_v4().simple().to_string(),
            group_name,
            invite_secret: Uuid::new_v4().simple().to_string(),
            node_id,
            nickname,
            control_key: new_control_key(),
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
        proof_for(&self.invite_secret, &self.group_id, node_id)
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
            control_key: new_control_key(),
        })
    }

    fn control_keypair(&self) -> identity::Keypair {
        identity::Keypair::from_protobuf_encoding(&self.control_key)
            .unwrap_or_else(|_| identity::Keypair::generate_ed25519())
    }

    pub(crate) fn ensure_control_key(&mut self) -> bool {
        if self.control_key.is_empty() {
            self.control_key = new_control_key();
            true
        } else {
            false
        }
    }
}

fn new_control_key() -> Vec<u8> {
    identity::Keypair::generate_ed25519()
        .to_protobuf_encoding()
        .expect("ed25519 keypair encoding must succeed")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LanAnnouncement {
    version: u8,
    group_id: String,
    node_id: String,
    nickname: String,
    group_name: String,
    proof: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LanPeer {
    pub node_id: String,
    pub nickname: String,
    pub group_name: String,
    pub address: String,
    pub port: u16,
    pub control_address: String,
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
    control_peer_id: PeerId,
}

#[derive(NetworkBehaviour)]
#[behaviour(to_swarm = "ControlEvent")]
struct ControlBehaviour {
    mdns: mdns::tokio::Behaviour,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
}

#[derive(Debug)]
enum ControlEvent {
    Mdns(MdnsEvent),
    Identify(identify::Event),
    Ping(ping::Event),
}

impl From<MdnsEvent> for ControlEvent {
    fn from(event: MdnsEvent) -> Self {
        Self::Mdns(event)
    }
}

impl From<identify::Event> for ControlEvent {
    fn from(event: identify::Event) -> Self {
        Self::Identify(event)
    }
}

impl From<ping::Event> for ControlEvent {
    fn from(event: ping::Event) -> Self {
        Self::Ping(event)
    }
}

pub(crate) struct LanRuntime {
    profile: RwLock<Option<LanProfile>>,
    peers: Arc<RwLock<HashMap<String, PeerRecord>>>,
    listener: Mutex<Option<JoinHandle<()>>>,
    dialer: Mutex<Option<UnboundedSender<Multiaddr>>>,
}

impl LanRuntime {
    pub(crate) fn new() -> Self {
        Self {
            profile: RwLock::new(None),
            peers: Arc::new(RwLock::new(HashMap::new())),
            listener: Mutex::new(None),
            dialer: Mutex::new(None),
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
        let own_group_id = profile.group_id.clone();
        let own_node_id = profile.node_id.clone();
        let invite_secret = profile.invite_secret.clone();
        let own_profile = profile.clone();
        let keypair = profile.control_keypair();
        let peer_id = PeerId::from(keypair.public());
        let announcement = serde_json::to_string(&LanAnnouncement {
            version: 1,
            group_id: profile.group_id.clone(),
            node_id: profile.node_id.clone(),
            nickname: profile.nickname.clone(),
            group_name: profile.group_name.clone(),
            proof: profile.proof_for(&profile.node_id),
        })?;
        let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)
            .map_err(|error| anyhow::anyhow!("无法启动局域网 mDNS：{error}"))?;
        let identify_config =
            identify::Config::new(LAN_PROTOCOL_VERSION.to_string(), keypair.public())
                .with_agent_version(format!("starbridge-lan {announcement}"));
        let mut swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_behaviour(|_| ControlBehaviour {
                mdns,
                identify: identify::Behaviour::new(identify_config),
                ping: ping::Behaviour::new(
                    ping::Config::new()
                        .with_interval(Duration::from_secs(5))
                        .with_timeout(Duration::from_secs(10)),
                ),
            })?
            .with_swarm_config(|config| {
                config.with_idle_connection_timeout(Duration::from_secs(PRESENCE_TIMEOUT_SECS))
            })
            .build();
        swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

        let peers = Arc::clone(&self.peers);
        let (dial_tx, mut dial_rx) = mpsc::unbounded_channel();
        let listener = tokio::spawn(async move {
            let mut discovered = HashMap::<PeerId, Multiaddr>::new();
            loop {
                let event = tokio::select! {
                    Some(address) = dial_rx.recv() => {
                        let _ = swarm.dial(address);
                        continue;
                    }
                    event = swarm.next() => event,
                };
                let Some(event) = event else {
                    break;
                };
                match event {
                    SwarmEvent::Behaviour(ControlEvent::Mdns(MdnsEvent::Discovered(items))) => {
                        tracing::debug!(count = items.len(), "LAN mDNS discovered peers");
                        for (peer, address) in items {
                            discovered.insert(peer, address.clone());
                            let _ = swarm.dial(address);
                        }
                    }
                    SwarmEvent::Behaviour(ControlEvent::Mdns(MdnsEvent::Expired(items))) => {
                        let mut records = peers.write().unwrap_or_else(|error| error.into_inner());
                        for (peer, address) in items {
                            if discovered.get(&peer) == Some(&address) {
                                discovered.remove(&peer);
                            }
                            records.retain(|_, record| record.control_peer_id != peer);
                        }
                    }
                    SwarmEvent::Behaviour(ControlEvent::Identify(identify::Event::Received {
                        peer_id,
                        info,
                        ..
                    })) => {
                        tracing::debug!(%peer_id, agent = %info.agent_version, "LAN identify received");
                        let Some(announcement) = parse_announcement(&info.agent_version) else {
                            let _ = swarm.disconnect_peer_id(peer_id);
                            continue;
                        };
                        let expected = proof_for(
                            &invite_secret,
                            &announcement.group_id,
                            &announcement.node_id,
                        );
                        if announcement.version != 1
                            || announcement.group_id != own_group_id
                            || announcement.node_id == own_node_id
                            || announcement.proof != expected
                        {
                            let _ = swarm.disconnect_peer_id(peer_id);
                            continue;
                        }
                        let Some(control_address) = discovered
                            .get(&peer_id)
                            .cloned()
                            .or_else(|| info.listen_addrs.first().cloned())
                        else {
                            continue;
                        };
                        let Some((address, port)) = socket_address(&control_address) else {
                            continue;
                        };
                        peers
                            .write()
                            .unwrap_or_else(|error| error.into_inner())
                            .insert(
                                announcement.node_id.clone(),
                                PeerRecord {
                                    peer: LanPeer {
                                        node_id: announcement.node_id,
                                        nickname: announcement.nickname,
                                        group_name: announcement.group_name,
                                        address,
                                        port,
                                        control_address: control_address.to_string(),
                                        online: true,
                                        last_seen: unix_now(),
                                    },
                                    control_peer_id: peer_id,
                                },
                            );
                    }
                    SwarmEvent::Behaviour(ControlEvent::Ping(ping::Event {
                        peer,
                        result: Ok(_),
                        ..
                    })) => {
                        let now = unix_now();
                        for record in peers
                            .write()
                            .unwrap_or_else(|error| error.into_inner())
                            .values_mut()
                        {
                            if record.control_peer_id == peer {
                                record.peer.last_seen = now;
                                record.peer.online = true;
                            }
                        }
                    }
                    SwarmEvent::ConnectionClosed { peer_id, .. } => {
                        peers
                            .write()
                            .unwrap_or_else(|error| error.into_inner())
                            .retain(|_, record| record.control_peer_id != peer_id);
                    }
                    SwarmEvent::NewListenAddr { address, .. } => {
                        tracing::debug!(%address, "LAN control listener ready");
                    }
                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        tracing::debug!(?peer_id, %error, "LAN control dial failed");
                    }
                    _ => {}
                }
            }
        });
        *self
            .profile
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(own_profile);
        *self
            .listener
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(listener);
        *self
            .dialer
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(dial_tx);
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
        self.dialer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(listener) = self
            .listener
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            listener.abort();
        }
        self.peers
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    pub(crate) fn dial_address(&self, address: &str) -> anyhow::Result<()> {
        let address: Multiaddr = address
            .parse()
            .map_err(|error| anyhow::anyhow!("手动设备地址无效：{error}"))?;
        let sender = self
            .dialer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| anyhow::anyhow!("局域网发现尚未启动"))?;
        sender
            .send(address)
            .map_err(|_| anyhow::anyhow!("局域网控制通道已停止"))
    }

    pub(crate) fn replace_profile(&self, profile: LanProfile) -> anyhow::Result<()> {
        self.start(profile)
    }

    pub(crate) fn resume(&self) -> anyhow::Result<()> {
        let Some(profile) = self.profile() else {
            return Ok(());
        };
        self.start(profile)
    }
}

fn parse_announcement(value: &str) -> Option<LanAnnouncement> {
    value
        .strip_prefix("starbridge-lan ")
        .and_then(|value| serde_json::from_str(value).ok())
}

fn socket_address(address: &Multiaddr) -> Option<(String, u16)> {
    let mut host = None;
    let mut port = None;
    for protocol in address {
        match protocol {
            libp2p::multiaddr::Protocol::Ip4(value) => host = Some(value.to_string()),
            libp2p::multiaddr::Protocol::Ip6(value) => host = Some(value.to_string()),
            libp2p::multiaddr::Protocol::Tcp(value) => port = Some(value),
            _ => {}
        }
    }
    Some((host?, port?))
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
        assert!(!profile.control_key.is_empty());
    }

    #[test]
    fn malformed_invite_is_rejected() {
        assert!(LanProfile::from_invite("SB1-NOT-A-CODE", "node".into(), "name".into()).is_err());
    }

    #[test]
    fn announcement_requires_group_proof() {
        let profile = LanProfile::new("group".into(), "node-a".into(), "a".into());
        let value = serde_json::to_string(&LanAnnouncement {
            version: 1,
            group_id: profile.group_id.clone(),
            node_id: profile.node_id.clone(),
            nickname: profile.nickname.clone(),
            group_name: profile.group_name.clone(),
            proof: profile.proof_for(&profile.node_id),
        })
        .unwrap();
        let parsed = parse_announcement(&format!("starbridge-lan {value}")).unwrap();
        assert_eq!(parsed.group_id, profile.group_id);
        assert_eq!(parsed.proof, profile.proof_for(&profile.node_id));
    }
}
