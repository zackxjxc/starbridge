use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}

pub fn new_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Peer,
    /// 兼容旧版设备状态；角色不再决定节点权限。
    Controller,
    Executor,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPolicy {
    #[default]
    DirectPreferred,
    DirectOnly,
    RelayOnly,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPath {
    #[default]
    Unknown,
    Direct,
    Relay,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    #[default]
    Negotiating,
    Connected,
    Failed,
    Closed,
}

/// 本机 SSH 服务进程拥有的权限，决定远程命令能做什么。
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SshPrivilege {
    #[default]
    Unknown,
    User,
    Admin,
    System,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AuthRequest {
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AuthResponse {
    pub access_token: String,
    pub username: String,
    pub expires_at: u64,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub refresh_expires_at: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AuthRefreshRequest {
    pub refresh_token: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AuthLogoutRequest {
    #[serde(default)]
    pub refresh_token: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DeviceMetadataUpdate {
    pub display_name: Option<String>,
    pub purpose: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceRegistration {
    pub node_id: String,
    pub device_token: String,
    /// Application-scoped machine lookup key; never an authentication credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_key: Option<String>,
    pub display_name: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub role: Role,
    pub ssh_alias: String,
    pub ssh_user: String,
    #[serde(default)]
    pub ssh_privilege: SshPrivilege,
    /// 本节点用于发起 SSH 登录的公钥。
    #[serde(default)]
    pub ssh_public_key: String,
    /// 本节点 sshd 的主机公钥，发起方据此校验目标身份。
    #[serde(default)]
    pub ssh_host_key: String,
    pub capabilities: Vec<String>,
    pub allow_relay: bool,
    pub direct_only: bool,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub hostname: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceRegistrationResponse {
    /// Canonical protocol record ID, including after authenticated reuse.
    pub node_id: String,
    pub devices: Vec<DeviceInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceRebindRequest {
    pub registration: DeviceRegistration,
    /// Fresh account verification, supplied only by an explicit user action.
    pub auth: AuthRequest,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrollmentChoice {
    #[default]
    Automatic,
    Add,
    Replace,
}

/// Passwords and recovery grants are transient and must never enter saved login state.
#[derive(Clone, Deserialize, Serialize)]
pub struct DeviceEnrollmentRequest {
    pub registration: DeviceRegistration,
    pub instance_id: String,
    #[serde(default)]
    pub replacement_token: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub grant: Option<String>,
    #[serde(default)]
    pub choice: EnrollmentChoice,
    #[serde(default)]
    pub target_node_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EnrollmentCandidate {
    pub node_id: String,
    pub display_name: String,
    pub online: bool,
    pub active_connections: usize,
    pub test_running: bool,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct DeviceEnrollmentResponse {
    pub registration: Option<DeviceRegistrationResponse>,
    pub device_token: Option<String>,
    pub grant: Option<String>,
    #[serde(default)]
    pub candidates: Vec<EnrollmentCandidate>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceInfo {
    pub node_id: String,
    pub account: String,
    pub display_name: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(default = "idle_occupancy")]
    pub occupancy: String,
    #[serde(default)]
    pub occupied_by: Option<String>,
    #[serde(default)]
    pub outgoing_count: usize,
    #[serde(default)]
    pub role: Role,
    pub ssh_alias: String,
    pub ssh_user: String,
    #[serde(default)]
    pub ssh_privilege: SshPrivilege,
    /// 本节点用于发起 SSH 登录的公钥。
    #[serde(default)]
    pub ssh_public_key: String,
    /// 本节点 sshd 的主机公钥，发起方据此校验目标身份。
    #[serde(default)]
    pub ssh_host_key: String,
    pub capabilities: Vec<String>,
    pub allow_relay: bool,
    pub direct_only: bool,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub hostname: String,
    pub online: bool,
    pub last_heartbeat: u64,
}

fn idle_occupancy() -> String { "idle".into() }

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HeartbeatRequest {
    pub node_id: String,
    pub device_token: String,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionCreateRequest {
    pub initiator_node_id: String,
    pub device_token: String,
    pub target_node_id: String,
    pub policy: ConnectionPolicy,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    #[serde(alias = "controller_node_id")]
    pub initiator_node_id: String,
    pub target_node_id: String,
    pub policy: ConnectionPolicy,
    pub path: SessionPath,
    pub status: SessionStatus,
    pub created_at: u64,
    pub expires_at: u64,
    pub failure_reason: Option<String>,
    pub route: Option<FrpRoute>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionReadiness {
    #[serde(flatten)]
    pub session: SessionInfo,
    /// Absent when the center does not observe managed FRPS proxy readiness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver_ready: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionHistoryEntry {
    pub session_id: String,
    pub peer_name: String,
    pub direction: String,
    pub status: SessionStatus,
    pub path: SessionPath,
    pub created_at: u64,
    pub failure_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionHistoryPage {
    pub items: Vec<SessionHistoryEntry>,
    pub total: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SessionHistoryQuery {
    #[serde(default)]
    pub offset: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FrpRoute {
    pub server_addr: String,
    pub server_port: u16,
    pub auth_token: String,
    pub proxy_name: String,
    pub secret_key: String,
    pub remote_port: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionStatusRequest {
    pub status: SessionStatus,
    pub path: SessionPath,
    pub failure_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiagnosticCreateRequest {
    pub node_id: String,
    pub device_token: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DiagnosticUpdateRequest {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub ssh_verified: bool,
    #[serde(default)]
    pub cleanup_complete: bool,
    pub outcome: Option<String>,
    pub failure_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DiagnosticSession {
    pub test_id: String,
    pub node_id: String,
    pub status: String,
    pub phase: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub challenge: String,
    pub route: FrpRoute,
    pub relay_verified: bool,
    pub ssh_verified: bool,
    pub cleanup_complete: bool,
    pub local_relay: bool,
    pub failure_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HealthResponse {
    pub service: &'static str,
    pub version: &'static str,
    pub now: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}
