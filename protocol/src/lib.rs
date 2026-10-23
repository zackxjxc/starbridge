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

/// 执行端 SSH 服务进程拥有的权限，决定远程命令能做什么。
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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeviceRegistration {
    pub node_id: String,
    pub device_token: String,
    pub display_name: String,
    pub role: Role,
    pub ssh_alias: String,
    pub ssh_user: String,
    #[serde(default)]
    pub ssh_privilege: SshPrivilege,
    /// 控制端用于登录执行端的 SSH 公钥。
    #[serde(default)]
    pub ssh_public_key: String,
    /// 执行端 sshd 的主机公钥，控制端据此校验主机身份。
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
pub struct DeviceInfo {
    pub node_id: String,
    pub account: String,
    pub display_name: String,
    pub role: Role,
    pub ssh_alias: String,
    pub ssh_user: String,
    #[serde(default)]
    pub ssh_privilege: SshPrivilege,
    /// 控制端用于登录执行端的 SSH 公钥。
    #[serde(default)]
    pub ssh_public_key: String,
    /// 执行端 sshd 的主机公钥，控制端据此校验主机身份。
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HeartbeatRequest {
    pub node_id: String,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionCreateRequest {
    pub target_node_id: String,
    pub policy: ConnectionPolicy,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub controller_node_id: String,
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
