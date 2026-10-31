use futures::StreamExt;
use libp2p::{
    identify, identity,
    mdns::{self, Event as MdnsEvent},
    noise, ping,
    request_response::{
        self, json, Event as RequestResponseEvent, Message as RequestResponseMessage,
        OutboundRequestId, ProtocolSupport,
    },
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, StreamProtocol, SwarmBuilder,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol as SocketProtocol, Socket as Socket2, Type};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket},
    str::FromStr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

const LAN_PROTOCOL_VERSION: &str = "/starbridge/lan/1";
const PRESENCE_TIMEOUT_SECS: u64 = 15;
const OCCUPANCY_TIMEOUT_SECS: u64 = 20;
const LAN_CONTROL_PROTOCOL: &str = "/starbridge/lan/control/1";
const LAN_DISCOVERY_MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 0, 1);
const LAN_DISCOVERY_PORT: u16 = 45_853;
const INVITE_CODE_LEN: usize = 6;
const INVITE_CODE_SPACE: u32 = 1_000_000;

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
    #[serde(default)]
    pub ssh_port: u16,
    #[serde(default)]
    pub ssh_user: String,
    #[serde(default)]
    pub ssh_public_key: String,
    #[serde(default)]
    pub ssh_host_key: String,
    #[serde(default)]
    pub discovery_paused: bool,
}

impl LanProfile {
    pub(crate) fn new(group_name: String, node_id: String, nickname: String) -> Self {
        let invite_secret = new_invite_secret();
        Self {
            group_id: group_id_for_invite_secret(&invite_secret),
            group_name,
            invite_secret,
            node_id,
            nickname,
            control_key: new_control_key(),
            ssh_port: 0,
            ssh_user: String::new(),
            ssh_public_key: String::new(),
            ssh_host_key: String::new(),
            discovery_paused: false,
        }
    }

    pub(crate) fn invite_code(&self) -> String {
        self.invite_secret.clone()
    }

    pub(crate) fn migrate_invite_code(&mut self) -> bool {
        let invite_secret = normalize_invite_secret(&self.invite_secret);
        let group_id = group_id_for_invite_secret(&invite_secret);
        let changed = self.invite_secret != invite_secret || self.group_id != group_id;
        self.invite_secret = invite_secret;
        self.group_id = group_id;
        changed
    }

    pub(crate) fn proof_for(&self, node_id: &str) -> String {
        proof_for(&self.invite_secret, &self.group_id, node_id)
    }

    pub(crate) fn parse_invite(code: &str) -> anyhow::Result<(String, String)> {
        let compact = code
            .trim()
            .replace(['-', ' ', '\n', '\r', '\t'], "");
        if compact.len() != INVITE_CODE_LEN || !compact.chars().all(|value| value.is_ascii_digit()) {
            anyhow::bail!("邀请码格式无效");
        }
        Ok((group_id_for_invite_secret(&compact), compact))
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
            ssh_port: 0,
            ssh_user: String::new(),
            ssh_public_key: String::new(),
            ssh_host_key: String::new(),
            discovery_paused: false,
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

fn new_invite_secret() -> String {
    let bytes = *Uuid::new_v4().as_bytes();
    let value = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % INVITE_CODE_SPACE;
    format!("{value:06}")
}

fn normalize_invite_secret(secret: &str) -> String {
    if secret.len() == INVITE_CODE_LEN && secret.chars().all(|value| value.is_ascii_digit()) {
        return secret.to_string();
    }
    let digest = Sha256::digest(secret.as_bytes());
    let value = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % INVITE_CODE_SPACE;
    format!("{value:06}")
}

fn group_id_for_invite_secret(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    digest[..16]
        .iter()
        .map(|value| format!("{value:02x}"))
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LanAnnouncement {
    version: u8,
    group_id: String,
    node_id: String,
    nickname: String,
    group_name: String,
    proof: String,
    ssh_port: u16,
    ssh_user: String,
    ssh_public_key: String,
    ssh_host_key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LanBeacon {
    version: u8,
    group_id: String,
    node_id: String,
    nickname: String,
    group_name: String,
    proof: String,
    peer_id: String,
    control_port: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LanPeer {
    pub node_id: String,
    pub nickname: String,
    pub group_name: String,
    pub address: String,
    pub port: u16,
    pub control_address: String,
    pub ssh_port: u16,
    pub ssh_user: String,
    pub ssh_public_key: String,
    pub ssh_host_key: String,
    pub occupied_by: Option<LanOccupancy>,
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
    pub occupancy: Option<LanOccupancy>,
    pub peers: Vec<LanPeer>,
    pub connections: Vec<LanConnection>,
    pub discovery: LanDiscoveryStatus,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct LanDiscoveryStatus {
    pub mdns_available: bool,
    pub fallback_available: bool,
    pub fallback_port: u16,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct LanConnection {
    pub node_id: String,
    pub nickname: String,
    pub ssh_command: String,
    pub state: String,
    pub connected_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LanOccupancy {
    pub holder_node_id: String,
    pub holder_nickname: String,
    pub since: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
enum LanControlRequest {
    Reserve {
        group_id: String,
        requester_node_id: String,
        requester_nickname: String,
        proof: String,
    },
    Release {
        group_id: String,
        requester_node_id: String,
        proof: String,
    },
    KeepAlive {
        group_id: String,
        requester_node_id: String,
        proof: String,
    },
    Status {
        group_id: String,
        node_id: String,
        proof: String,
        occupancy: Option<LanOccupancy>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct LanControlResponse {
    accepted: bool,
    message: String,
    occupancy: Option<LanOccupancy>,
}

type LanControlBehaviour = json::Behaviour<LanControlRequest, LanControlResponse>;
type LanControlEvent = RequestResponseEvent<LanControlRequest, LanControlResponse>;

#[derive(Debug)]
enum LanCommand {
    Dial(Multiaddr),
    Reserve {
        node_id: String,
        response: oneshot::Sender<anyhow::Result<LanOccupancy>>,
    },
    Release {
        node_id: String,
        response: oneshot::Sender<anyhow::Result<()>>,
    },
}

#[derive(Clone)]
struct LeaseState {
    view: LanOccupancy,
    last_seen: u64,
}

#[derive(Clone)]
struct PeerRecord {
    peer: LanPeer,
    control_peer_id: PeerId,
}

#[derive(NetworkBehaviour)]
#[behaviour(to_swarm = "ControlEvent")]
struct ControlBehaviour {
    mdns: Toggle<mdns::tokio::Behaviour>,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    control: LanControlBehaviour,
}

#[derive(Debug)]
enum ControlEvent {
    Mdns(MdnsEvent),
    Identify(identify::Event),
    Ping(ping::Event),
    Control(LanControlEvent),
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

impl From<LanControlEvent> for ControlEvent {
    fn from(event: LanControlEvent) -> Self {
        Self::Control(event)
    }
}

pub(crate) struct LanRuntime {
    profile: RwLock<Option<LanProfile>>,
    peers: Arc<RwLock<HashMap<String, PeerRecord>>>,
    occupancy: Arc<RwLock<Option<LeaseState>>>,
    listener: Mutex<Option<JoinHandle<()>>>,
    commands: Mutex<Option<UnboundedSender<LanCommand>>>,
    discovery: RwLock<LanDiscoveryStatus>,
}

impl LanRuntime {
    pub(crate) fn new() -> Self {
        Self {
            profile: RwLock::new(None),
            peers: Arc::new(RwLock::new(HashMap::new())),
            occupancy: Arc::new(RwLock::new(None)),
            listener: Mutex::new(None),
            commands: Mutex::new(None),
            discovery: RwLock::new(LanDiscoveryStatus::default()),
        }
    }

    pub(crate) fn profile(&self) -> Option<LanProfile> {
        self.profile.read().ok().and_then(|profile| profile.clone())
    }

    pub(crate) fn discovery_running(&self) -> bool {
        self.listener
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }

    pub(crate) fn snapshot(&self) -> LanSnapshot {
        let profile = self.profile();
        let now = unix_now();
        let occupancy = {
            let current = self
                .occupancy
                .write()
                .unwrap_or_else(|error| error.into_inner());
            current.as_ref().map(|lease| lease.view.clone())
        };
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
            occupancy,
            peers: items,
            connections: Vec::new(),
            discovery: self
                .discovery
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        }
    }

    pub(crate) fn start(&self, profile: LanProfile) -> anyhow::Result<()> {
        self.stop_discovery();
        let own_group_id = profile.group_id.clone();
        let own_node_id = profile.node_id.clone();
        let own_nickname = profile.nickname.clone();
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
            ssh_port: profile.ssh_port,
            ssh_user: profile.ssh_user.clone(),
            ssh_public_key: profile.ssh_public_key.clone(),
            ssh_host_key: profile.ssh_host_key.clone(),
        })?;
        let (mdns, mdns_available, mdns_error) =
            match mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id) {
                Ok(behaviour) => (Toggle::from(Some(behaviour)), true, None),
                Err(error) => {
                    tracing::warn!(%error, "局域网 mDNS 不可用，将使用 StarBridge 备用发现");
                    (Toggle::from(None), false, Some(error.to_string()))
                }
            };
        let fallback_socket = match create_fallback_discovery_socket() {
            Ok(socket) => Some(socket),
            Err(error) => {
                tracing::warn!(%error, port = LAN_DISCOVERY_PORT, "StarBridge 备用局域网发现不可用");
                None
            }
        };
        let discovery_message = match (mdns_error, fallback_socket.is_some()) {
            (Some(error), true) => Some(format!("mDNS 不可用，已切换 StarBridge 备用发现：{error}")),
            (Some(error), false) => Some(format!("自动发现不可用：mDNS 和备用发现均失败：{error}")),
            (None, false) => Some(format!("备用发现端口 {LAN_DISCOVERY_PORT} 不可用，仍可使用 mDNS 或手动添加")),
            (None, true) => None,
        };
        *self
            .discovery
            .write()
            .unwrap_or_else(|error| error.into_inner()) = LanDiscoveryStatus {
            mdns_available,
            fallback_available: fallback_socket.is_some(),
            fallback_port: LAN_DISCOVERY_PORT,
            message: discovery_message,
        };
        let identify_config =
            identify::Config::new(LAN_PROTOCOL_VERSION.to_string(), keypair.public())
                .with_agent_version(format!("starbridge-lan {announcement}"));
        let control = LanControlBehaviour::new(
            [(
                StreamProtocol::new(LAN_CONTROL_PROTOCOL),
                ProtocolSupport::Full,
            )],
            request_response::Config::default(),
        );
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
                control,
            })?
            .with_swarm_config(|config| {
                // Ping is the liveness signal. Keep the libp2p connection alive
                // long enough for a missed ping to be handled by the presence TTL.
                config.with_idle_connection_timeout(Duration::from_secs(60 * 60))
            })
            .build();
        swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

        let peers = Arc::clone(&self.peers);
        let occupancy = Arc::clone(&self.occupancy);
        let fallback_group_name = own_profile.group_name.clone();
        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let listener = tokio::spawn(async move {
            let fallback_socket = fallback_socket;
            let fallback_beacon = LanBeacon {
                version: 1,
                group_id: own_group_id.clone(),
                node_id: own_node_id.clone(),
                nickname: own_nickname.clone(),
                group_name: fallback_group_name,
                proof: proof_for(&invite_secret, &own_group_id, &own_node_id),
                peer_id: peer_id.to_string(),
                control_port: 0,
            };
            let mut discovered = HashMap::<PeerId, Multiaddr>::new();
            let mut connected_addresses = HashMap::<PeerId, Multiaddr>::new();
            let mut pending = HashMap::<OutboundRequestId, LanCommand>::new();
            let mut outgoing = HashMap::<String, (PeerId, LanOccupancy)>::new();
            let mut keepalive = tokio::time::interval(Duration::from_secs(5));
            let mut beacon_tick = tokio::time::interval(Duration::from_secs(3));
            let mut control_port = None;
            let mut fallback_buffer = vec![0u8; 4096];
            loop {
                let event = tokio::select! {
                    Some(command) = command_rx.recv() => {
                        match command {
                            LanCommand::Dial(address) => { let _ = swarm.dial(address); }
                            LanCommand::Reserve { node_id, response } => {
                                let Some(record) = peers.read().unwrap_or_else(|error| error.into_inner()).get(&node_id).cloned() else {
                                    let _ = response.send(Err(anyhow::anyhow!("设备当前离线，已从列表移除")));
                                    continue;
                                };
                                let request = LanControlRequest::Reserve {
                                    group_id: own_group_id.clone(),
                                    requester_node_id: own_node_id.clone(),
                                    requester_nickname: own_nickname.clone(),
                                    proof: proof_for(&invite_secret, &own_group_id, &own_node_id),
                                };
                                let request_id = swarm.behaviour_mut().control.send_request(&record.control_peer_id, request);
                                pending.insert(request_id, LanCommand::Reserve { node_id, response });
                            }
                            LanCommand::Release { node_id, response } => {
                                if let Some((peer_id, _)) = outgoing.remove(&node_id) {
                                    let request = LanControlRequest::Release {
                                        group_id: own_group_id.clone(),
                                        requester_node_id: own_node_id.clone(),
                                        proof: proof_for(&invite_secret, &own_group_id, &own_node_id),
                                    };
                                    let request_id = swarm.behaviour_mut().control.send_request(&peer_id, request);
                                    pending.insert(request_id, LanCommand::Release { node_id, response });
                                } else {
                                    let _ = response.send(Ok(()));
                                }
                            }
                        }
                        continue;
                    }
                    _ = keepalive.tick() => {
                        let expired = occupancy
                            .read()
                            .unwrap_or_else(|error| error.into_inner())
                            .as_ref()
                            .map(|lease| unix_now().saturating_sub(lease.last_seen) > OCCUPANCY_TIMEOUT_SECS)
                            .unwrap_or(false);
                        if expired {
                            *occupancy
                                .write()
                                .unwrap_or_else(|error| error.into_inner()) = None;
                            let status = LanControlRequest::Status {
                                group_id: own_group_id.clone(),
                                node_id: own_node_id.clone(),
                                proof: proof_for(&invite_secret, &own_group_id, &own_node_id),
                                occupancy: None,
                            };
                            let targets: Vec<_> = peers
                                .read()
                                .unwrap_or_else(|error| error.into_inner())
                                .values()
                                .map(|record| record.control_peer_id)
                                .collect();
                            for target in targets {
                                let _ = swarm.behaviour_mut().control.send_request(&target, status.clone());
                            }
                        }
                        for (peer_id, _) in outgoing.values() {
                            let request = LanControlRequest::KeepAlive {
                                group_id: own_group_id.clone(),
                                requester_node_id: own_node_id.clone(),
                                proof: proof_for(&invite_secret, &own_group_id, &own_node_id),
                            };
                            let _ = swarm.behaviour_mut().control.send_request(peer_id, request);
                        }
                        continue;
                    }
                    _ = beacon_tick.tick(), if fallback_socket.is_some() => {
                        if let (Some(socket), Some(port)) = (fallback_socket.as_ref(), control_port) {
                            let mut beacon = fallback_beacon.clone();
                            beacon.control_port = port;
                            if let Ok(payload) = serde_json::to_vec(&beacon) {
                                let target = SocketAddr::V4(SocketAddrV4::new(
                                    LAN_DISCOVERY_MULTICAST,
                                    LAN_DISCOVERY_PORT,
                                ));
                                let _ = socket.send_to(&payload, target).await;
                            }
                        }
                        continue;
                    }
                    result = async {
                        match fallback_socket.as_ref() {
                            Some(socket) => Some(socket.recv_from(&mut fallback_buffer).await),
                            None => None,
                        }
                    }, if fallback_socket.is_some() => {
                        let Some(Ok((size, source))) = result else { continue; };
                        let Ok(beacon) = serde_json::from_slice::<LanBeacon>(&fallback_buffer[..size]) else {
                            continue;
                        };
                        if beacon.version != 1
                            || beacon.group_id != own_group_id
                            || beacon.node_id == own_node_id
                            || beacon.control_port == 0
                            || beacon.proof != proof_for(&invite_secret, &beacon.group_id, &beacon.node_id)
                        {
                            continue;
                        }
                        let Ok(peer_id) = PeerId::from_str(&beacon.peer_id) else {
                            continue;
                        };
                        let IpAddr::V4(ip) = source.ip() else {
                            continue;
                        };
                        let address: Multiaddr = format!(
                            "/ip4/{ip}/tcp/{}/p2p/{peer_id}",
                            beacon.control_port
                        )
                        .parse()
                        .expect("validated StarBridge fallback address");
                        let changed = discovered.get(&peer_id) != Some(&address);
                        discovered.insert(peer_id, address.clone());
                        if changed && !connected_addresses.contains_key(&peer_id) {
                            let _ = swarm.dial(address);
                        }
                        continue;
                    }
                    event = swarm.next() => event,
                };
                let Some(event) = event else {
                    break;
                };
                match event {
                    SwarmEvent::ConnectionEstablished {
                        peer_id, endpoint, ..
                    } => {
                        connected_addresses
                            .insert(peer_id, endpoint.get_remote_address().clone());
                    }
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
                            tracing::warn!(%peer_id, "LAN identify announcement could not be parsed");
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
                            tracing::warn!(%peer_id, "LAN peer group proof rejected");
                            let _ = swarm.disconnect_peer_id(peer_id);
                            continue;
                        }
                        let Some(control_address) = connected_addresses
                            .get(&peer_id)
                            .cloned()
                            .or_else(|| discovered
                            .get(&peer_id)
                            .cloned()
                            .or_else(|| info.listen_addrs.first().cloned()))
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
                                        ssh_port: announcement.ssh_port,
                                        ssh_user: announcement.ssh_user,
                                        ssh_public_key: announcement.ssh_public_key,
                                        ssh_host_key: announcement.ssh_host_key,
                                        occupied_by: None,
                                        online: true,
                                        last_seen: unix_now(),
                                    },
                                    control_peer_id: peer_id,
                                },
                            );
                        let status = LanControlRequest::Status {
                            group_id: own_group_id.clone(),
                            node_id: own_node_id.clone(),
                            proof: proof_for(&invite_secret, &own_group_id, &own_node_id),
                            occupancy: occupancy_view(&occupancy),
                        };
                        let _ = swarm.behaviour_mut().control.send_request(&peer_id, status);
                    }
                    SwarmEvent::Behaviour(ControlEvent::Control(event)) => match event {
                        RequestResponseEvent::Message { peer, message, .. } => match message {
                            RequestResponseMessage::Request {
                                request, channel, ..
                            } => {
                                let response = handle_incoming_control_request(
                                    &peers,
                                    &occupancy,
                                    peer,
                                    request,
                                    &own_group_id,
                                    &invite_secret,
                                );
                                let changed = response.1;
                                let _ = swarm
                                    .behaviour_mut()
                                    .control
                                    .send_response(channel, response.0);
                                if changed {
                                    let status = LanControlRequest::Status {
                                        group_id: own_group_id.clone(),
                                        node_id: own_node_id.clone(),
                                        proof: proof_for(
                                            &invite_secret,
                                            &own_group_id,
                                            &own_node_id,
                                        ),
                                        occupancy: occupancy_view(&occupancy),
                                    };
                                    let targets: Vec<_> = peers
                                        .read()
                                        .unwrap_or_else(|error| error.into_inner())
                                        .values()
                                        .map(|record| record.control_peer_id)
                                        .filter(|target| *target != peer)
                                        .collect();
                                    for target in targets {
                                        let _ = swarm
                                            .behaviour_mut()
                                            .control
                                            .send_request(&target, status.clone());
                                    }
                                }
                            }
                            RequestResponseMessage::Response {
                                request_id,
                                response,
                            } => {
                                if let Some(command) = pending.remove(&request_id) {
                                    let accepted = response.accepted;
                                    let message = response.message.clone();
                                    match command {
                                        LanCommand::Reserve {
                                            node_id,
                                            response: reply,
                                        } => {
                                            if accepted {
                                                let lease =
                                                    response.occupancy.unwrap_or(LanOccupancy {
                                                        holder_node_id: own_node_id.clone(),
                                                        holder_nickname: own_nickname.clone(),
                                                        since: unix_now(),
                                                    });
                                                outgoing
                                                    .insert(node_id.clone(), (peer, lease.clone()));
                                                if let Some(record) = peers
                                                    .write()
                                                    .unwrap_or_else(|error| error.into_inner())
                                                    .get_mut(&node_id)
                                                {
                                                    record.peer.occupied_by = Some(lease.clone());
                                                }
                                                let _ = reply.send(Ok(lease));
                                            } else {
                                                let _ = reply.send(Err(anyhow::anyhow!(message)));
                                            }
                                        }
                                        LanCommand::Release {
                                            node_id,
                                            response: reply,
                                        } => {
                                            if let Some(record) = peers
                                                .write()
                                                .unwrap_or_else(|error| error.into_inner())
                                                .get_mut(&node_id)
                                            {
                                                record.peer.occupied_by = None;
                                            }
                                            let _ = reply.send(if accepted {
                                                Ok(())
                                            } else {
                                                Err(anyhow::anyhow!(message))
                                            });
                                        }
                                        LanCommand::Dial(_) => {}
                                    }
                                } else {
                                    update_peer_occupancy(&peers, peer, response.occupancy);
                                }
                            }
                        },
                        RequestResponseEvent::OutboundFailure {
                            request_id, error, ..
                        } => {
                            if let Some(command) = pending.remove(&request_id) {
                                let message = anyhow::anyhow!("局域网控制请求失败：{error}");
                                match command {
                                    LanCommand::Reserve { response, .. } => {
                                        let _ = response.send(Err(message));
                                    }
                                    LanCommand::Release { response, .. } => {
                                        let _ = response.send(Err(message));
                                    }
                                    LanCommand::Dial(_) => {}
                                }
                            }
                        }
                        _ => {}
                    },
                    SwarmEvent::Behaviour(ControlEvent::Ping(ping::Event {
                        peer,
                        result: Ok(_),
                        ..
                    })) => {
                        tracing::debug!(%peer, "LAN ping succeeded");
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
                        tracing::debug!(%peer_id, "LAN control connection closed");
                        connected_addresses.remove(&peer_id);
                        let disconnected_nodes: Vec<_> = peers
                            .read()
                            .unwrap_or_else(|error| error.into_inner())
                            .values()
                            .filter(|record| record.control_peer_id == peer_id)
                            .map(|record| record.peer.node_id.clone())
                            .collect();
                        peers
                            .write()
                            .unwrap_or_else(|error| error.into_inner())
                            .retain(|_, record| record.control_peer_id != peer_id);
                        if occupancy_holder_matches_peer(&occupancy, &disconnected_nodes) {
                            *occupancy.write().unwrap_or_else(|error| error.into_inner()) = None;
                            let status = LanControlRequest::Status {
                                group_id: own_group_id.clone(),
                                node_id: own_node_id.clone(),
                                proof: proof_for(&invite_secret, &own_group_id, &own_node_id),
                                occupancy: None,
                            };
                            let targets: Vec<_> = peers
                                .read()
                                .unwrap_or_else(|error| error.into_inner())
                                .values()
                                .map(|record| record.control_peer_id)
                                .collect();
                            for target in targets {
                                let _ = swarm
                                    .behaviour_mut()
                                    .control
                                    .send_request(&target, status.clone());
                            }
                        }
                        outgoing.retain(|node_id, (target, _)| {
                            if *target == peer_id {
                                tracing::debug!(%node_id, "LAN outbound occupancy released after control disconnect");
                                false
                            } else {
                                true
                            }
                        });
                    }
                    SwarmEvent::NewListenAddr { address, .. } => {
                        tracing::debug!(%address, "LAN control listener ready");
                        if control_port.is_none() {
                            control_port = socket_address(&address).map(|(_, port)| port);
                        }
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
            .commands
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(command_tx);
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
        self.commands
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
        *self
            .occupancy
            .write()
            .unwrap_or_else(|error| error.into_inner()) = None;
        *self
            .discovery
            .write()
            .unwrap_or_else(|error| error.into_inner()) = LanDiscoveryStatus::default();
    }

    pub(crate) fn dial_address(&self, address: &str) -> anyhow::Result<()> {
        let address: Multiaddr = address
            .parse()
            .map_err(|error| anyhow::anyhow!("手动设备地址无效：{error}"))?;
        let sender = self
            .commands
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| anyhow::anyhow!("局域网发现尚未启动"))?;
        sender
            .send(LanCommand::Dial(address))
            .map_err(|_| anyhow::anyhow!("局域网控制通道已停止"))
    }

    pub(crate) fn occupancy(&self) -> Option<LanOccupancy> {
        self.snapshot().occupancy
    }

    pub(crate) async fn reserve_peer(&self, node_id: &str) -> anyhow::Result<LanOccupancy> {
        let sender = self
            .commands
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| anyhow::anyhow!("局域网控制通道已停止"))?;
        let (response_tx, response_rx) = oneshot::channel();
        sender
            .send(LanCommand::Reserve {
                node_id: node_id.to_string(),
                response: response_tx,
            })
            .map_err(|_| anyhow::anyhow!("局域网控制通道已停止"))?;
        tokio::time::timeout(Duration::from_secs(5), response_rx)
            .await
            .map_err(|_| anyhow::anyhow!("等待局域网设备占用确认超时"))?
            .map_err(|_| anyhow::anyhow!("局域网设备占用确认通道已关闭"))?
    }

    pub(crate) async fn release_peer(&self, node_id: &str) -> anyhow::Result<()> {
        let sender = self
            .commands
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| anyhow::anyhow!("局域网控制通道已停止"))?;
        let (response_tx, response_rx) = oneshot::channel();
        sender
            .send(LanCommand::Release {
                node_id: node_id.to_string(),
                response: response_tx,
            })
            .map_err(|_| anyhow::anyhow!("局域网控制通道已停止"))?;
        tokio::time::timeout(Duration::from_secs(5), response_rx)
            .await
            .map_err(|_| anyhow::anyhow!("等待局域网设备释放超时"))?
            .map_err(|_| anyhow::anyhow!("局域网设备释放通道已关闭"))?
    }

    pub(crate) fn replace_profile(&self, profile: LanProfile) -> anyhow::Result<()> {
        self.start(profile)
    }

    pub(crate) fn set_profile(&self, profile: LanProfile) {
        *self
            .profile
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(profile);
    }

    pub(crate) fn resume(&self) -> anyhow::Result<()> {
        let Some(profile) = self.profile() else {
            return Ok(());
        };
        self.start(profile)
    }
}

fn create_fallback_discovery_socket() -> anyhow::Result<UdpSocket> {
    let socket = Socket2::new(Domain::IPV4, Type::DGRAM, Some(SocketProtocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(
        &SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, LAN_DISCOVERY_PORT)
            .into(),
    )?;
    socket.join_multicast_v4(&LAN_DISCOVERY_MULTICAST, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_nonblocking(true)?;
    let socket: StdUdpSocket = socket.into();
    Ok(UdpSocket::from_std(socket)?)
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

fn occupancy_view(occupancy: &Arc<RwLock<Option<LeaseState>>>) -> Option<LanOccupancy> {
    occupancy
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|lease| lease.view.clone())
}

fn occupancy_holder_matches_peer(
    occupancy: &Arc<RwLock<Option<LeaseState>>>,
    node_ids: &[String],
) -> bool {
    occupancy
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|lease| {
            node_ids
                .iter()
                .any(|node_id| node_id == &lease.view.holder_node_id)
        })
        .unwrap_or(false)
}

fn update_peer_occupancy(
    peers: &Arc<RwLock<HashMap<String, PeerRecord>>>,
    control_peer_id: PeerId,
    occupancy: Option<LanOccupancy>,
) {
    for record in peers
        .write()
        .unwrap_or_else(|error| error.into_inner())
        .values_mut()
    {
        if record.control_peer_id == control_peer_id {
            record.peer.occupied_by = occupancy.clone();
        }
    }
}

fn handle_incoming_control_request(
    peers: &Arc<RwLock<HashMap<String, PeerRecord>>>,
    occupancy: &Arc<RwLock<Option<LeaseState>>>,
    control_peer_id: PeerId,
    request: LanControlRequest,
    own_group_id: &str,
    invite_secret: &str,
) -> (LanControlResponse, bool) {
    let now = unix_now();
    let peer_node_id = peers
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .find(|record| record.control_peer_id == control_peer_id)
        .map(|record| record.peer.node_id.clone());
    let Some(peer_node_id) = peer_node_id else {
        return (
            LanControlResponse {
                accepted: false,
                message: "局域网设备尚未完成身份握手".into(),
                occupancy: occupancy_view(occupancy),
            },
            false,
        );
    };
    let proof_valid = |node_id: &str, proof: &str| {
        node_id == peer_node_id && proof_for(invite_secret, own_group_id, node_id) == proof
    };
    let mut changed = false;
    let response = match request {
        LanControlRequest::Reserve {
            group_id,
            requester_node_id,
            requester_nickname,
            proof,
        } => {
            if group_id != own_group_id || !proof_valid(&requester_node_id, &proof) {
                LanControlResponse {
                    accepted: false,
                    message: "局域网组凭据无效".into(),
                    occupancy: occupancy_view(occupancy),
                }
            } else {
                let mut current = occupancy.write().unwrap_or_else(|error| error.into_inner());
                if let Some(lease) = current.as_mut() {
                    if lease.view.holder_node_id == requester_node_id {
                        lease.last_seen = now;
                        LanControlResponse {
                            accepted: true,
                            message: "设备已由本机占用".into(),
                            occupancy: Some(lease.view.clone()),
                        }
                    } else {
                        LanControlResponse {
                            accepted: false,
                            message: format!("设备已被 {} 连接", lease.view.holder_nickname),
                            occupancy: Some(lease.view.clone()),
                        }
                    }
                } else {
                    let view = LanOccupancy {
                        holder_node_id: requester_node_id,
                        holder_nickname: requester_nickname,
                        since: now,
                    };
                    *current = Some(LeaseState {
                        view: view.clone(),
                        last_seen: now,
                    });
                    changed = true;
                    LanControlResponse {
                        accepted: true,
                        message: "设备占用成功".into(),
                        occupancy: Some(view),
                    }
                }
            }
        }
        LanControlRequest::Release {
            group_id,
            requester_node_id,
            proof,
        } => {
            if group_id != own_group_id || !proof_valid(&requester_node_id, &proof) {
                LanControlResponse {
                    accepted: false,
                    message: "局域网组凭据无效".into(),
                    occupancy: occupancy_view(occupancy),
                }
            } else {
                let mut current = occupancy.write().unwrap_or_else(|error| error.into_inner());
                if current
                    .as_ref()
                    .map(|lease| lease.view.holder_node_id == requester_node_id)
                    .unwrap_or(false)
                {
                    *current = None;
                    changed = true;
                    LanControlResponse {
                        accepted: true,
                        message: "设备占用已释放".into(),
                        occupancy: None,
                    }
                } else {
                    LanControlResponse {
                        accepted: true,
                        message: "设备占用已经释放".into(),
                        occupancy: occupancy_view(occupancy),
                    }
                }
            }
        }
        LanControlRequest::KeepAlive {
            group_id,
            requester_node_id,
            proof,
        } => {
            if group_id != own_group_id || !proof_valid(&requester_node_id, &proof) {
                LanControlResponse {
                    accepted: false,
                    message: "局域网组凭据无效".into(),
                    occupancy: occupancy_view(occupancy),
                }
            } else {
                let mut current = occupancy.write().unwrap_or_else(|error| error.into_inner());
                if let Some(lease) = current.as_mut() {
                    if lease.view.holder_node_id == requester_node_id {
                        lease.last_seen = now;
                    }
                }
                LanControlResponse {
                    accepted: true,
                    message: "占用仍然有效".into(),
                    occupancy: current.as_ref().map(|lease| lease.view.clone()),
                }
            }
        }
        LanControlRequest::Status {
            group_id,
            node_id,
            proof,
            occupancy: remote_occupancy,
        } => {
            if group_id != own_group_id || !proof_valid(&node_id, &proof) {
                LanControlResponse {
                    accepted: false,
                    message: "局域网组凭据无效".into(),
                    occupancy: occupancy_view(occupancy),
                }
            } else {
                update_peer_occupancy(peers, control_peer_id, remote_occupancy);
                LanControlResponse {
                    accepted: true,
                    message: "状态已同步".into(),
                    occupancy: occupancy_view(occupancy),
                }
            }
        }
    };
    (response, changed)
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
        assert_eq!(code.len(), INVITE_CODE_LEN);
        assert!(code.chars().all(|value| value.is_ascii_digit()));
        let joined = LanProfile::from_invite(&code, "node-b".into(), "测试机".into()).unwrap();
        assert_eq!(joined.group_id, profile.group_id);
        assert_eq!(joined.invite_secret, profile.invite_secret);
        assert_ne!(profile.proof_for("node-a"), profile.invite_secret);
        assert!(!profile.control_key.is_empty());
    }

    #[test]
    fn malformed_invite_is_rejected() {
        assert!(LanProfile::from_invite("12345", "node".into(), "name".into()).is_err());
        assert!(LanProfile::from_invite("12345A", "node".into(), "name".into()).is_err());
    }

    #[test]
    fn legacy_invite_profile_migrates_to_a_stable_six_digit_code() {
        let mut profile = LanProfile::new("group".into(), "node-a".into(), "a".into());
        profile.invite_secret = "legacy-secret".into();
        profile.group_id = "legacy-group".into();
        assert!(profile.migrate_invite_code());
        assert_eq!(profile.invite_code().len(), INVITE_CODE_LEN);
        assert!(profile.invite_code().chars().all(|value| value.is_ascii_digit()));
        assert_eq!(profile.group_id, group_id_for_invite_secret(&profile.invite_secret));
        assert!(!profile.migrate_invite_code());
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
            ssh_port: profile.ssh_port,
            ssh_user: profile.ssh_user.clone(),
            ssh_public_key: profile.ssh_public_key.clone(),
            ssh_host_key: profile.ssh_host_key.clone(),
        })
        .unwrap();
        let parsed = parse_announcement(&format!("starbridge-lan {value}")).unwrap();
        assert_eq!(parsed.group_id, profile.group_id);
        assert_eq!(parsed.proof, profile.proof_for(&profile.node_id));
    }
}
