use super::*;

#[derive(Clone, Debug, Serialize)]
pub struct DeviceOperation {
    pub node_id: String,
    pub state: String,
    pub stage: String,
    pub message: String,
    pub session_id: Option<String>,
    pub ssh_ready: bool,
    pub path: SessionPath,
    #[serde(skip)] scheduled: bool,
}

impl DeviceOperation {
    fn queued(node_id: String) -> Self {
        Self { node_id, state: "queued".into(), stage: "queued".into(), message: "等待连接".into(),
            session_id: None, ssh_ready: false, path: SessionPath::Unknown, scheduled: false }
    }
}

#[derive(Clone)]
pub(super) struct BatchRecord { pub id: String, pub targets: Vec<String>, pub policy: ConnectionPolicy, pub cancelled: bool }

#[derive(Clone, Debug, Serialize)]
pub struct BatchView {
    pub id: String, pub running: bool, pub total: usize, pub completed: usize,
    pub direct_ready: usize, pub relay_ready: usize, pub failed: usize, pub skipped: usize,
    pub retained_relay: usize, pub items: Vec<DeviceOperation>,
}

#[derive(Debug, Deserialize)]
pub struct BatchConnectForm {
    pub policy: ConnectionPolicy,
    #[serde(default)] pub rebuild_relay: bool,
}
#[derive(Debug, Deserialize)]
pub struct TerminateForm { pub target_node_id: Option<String> }
#[derive(Debug, Deserialize)]
pub struct ExportForm { #[serde(default)] pub target_node_ids: Vec<String> }

pub(super) fn batch_view(inner: &Inner) -> Option<BatchView> {
    let batch = inner.batch.as_ref()?;
    let items: Vec<_> = batch.targets.iter().filter_map(|id| inner.operations.get(id).cloned()).collect();
    let pending = items.iter().filter(|item| operation_running(&item.state)).count();
    Some(BatchView {
        id: batch.id.clone(), running: pending > 0, total: batch.targets.len(),
        completed: items.len() - pending,
        direct_ready: items.iter().filter(|item| item.ssh_ready && item.path == SessionPath::Direct).count(),
        relay_ready: items.iter().filter(|item| item.ssh_ready && item.path == SessionPath::Relay
            && batch.policy != ConnectionPolicy::DirectOnly).count(),
        retained_relay: items.iter().filter(|item| item.state == "retained_relay").count(),
        failed: items.iter().filter(|item| item.state == "failed").count(),
        skipped: items.iter().filter(|item| matches!(item.state.as_str(), "skipped" | "terminated")).count(),
        items,
    })
}

fn operation_running(state: &str) -> bool { matches!(state, "queued" | "connecting" | "verifying") }

pub(super) async fn connect(State(app): State<ClientApp>, Json(form): Json<ConnectForm>)
    -> Result<Json<ClientSnapshot>, ApiError>
{
    app.enqueue_connection(form.target_node_id, form.policy, false, None).await?;
    Ok(Json(snapshot(&*app.inner.read().await)))
}

pub(super) async fn connect_all(State(app): State<ClientApp>, Json(form): Json<BatchConnectForm>)
    -> Result<Json<ClientSnapshot>, ApiError>
{
    let (ids, batch_id) = {
        let mut inner = app.inner.write().await;
        if inner.auth.is_none() || inner.closing_account { return Err(ApiError::unauthorized("请先登录")); }
        if matches!(inner.self_test.state.as_str(), "running" | "cleaning") {
            return Err(ApiError::bad_request("本机正在进行连接测试"));
        }
        if batch_view(&inner).is_some_and(|batch| batch.running) { return Ok(Json(snapshot(&inner))); }
        if inner.sessions.values().any(|session| session.target_node_id == inner.identity.node_id) {
            return Err(ApiError::bad_request("本机正在被其他设备连接，请先终止该连接"));
        }
        let ids = inner.devices.iter().filter(|device| device.node_id != inner.identity.node_id)
            .map(|device| device.node_id.clone()).collect::<Vec<_>>();
        let batch_id = starbridge_protocol::new_id();
        inner.batch = Some(BatchRecord { id: batch_id.clone(), targets: ids.clone(), policy: form.policy, cancelled: false });
        for id in &ids {
            inner.blocked_peers.remove(id);
            if !inner.operations.get(id).is_some_and(|operation| operation_running(&operation.state)) {
                inner.operations.insert(id.clone(), DeviceOperation::queued(id.clone()));
            }
        }
        inner.config.connection_policy = form.policy;
        inner.config.rebuild_relay = form.rebuild_relay;
        save_json(&inner.root.join("config.json"), &inner.config).await.map_err(ApiError::internal)?;
        (ids, batch_id)
    };
    for id in ids {
        if let Err(error) = app.enqueue_connection(id.clone(), form.policy, form.rebuild_relay, Some(&batch_id)).await {
            let mut inner = app.inner.write().await;
            if inner.blocked_peers.contains(&id) || inner.batch.as_ref().is_none_or(|batch|batch.id!=batch_id||batch.cancelled) { continue; }
            let mut operation = DeviceOperation::queued(id.clone());
            operation.state = "skipped".into(); operation.stage = "admission".into(); operation.message = error.message;
            inner.operations.insert(id, operation);
        }
    }
    Ok(Json(snapshot(&*app.inner.read().await)))
}

pub(super) async fn terminate(State(app): State<ClientApp>, Json(form): Json<TerminateForm>)
    -> Result<Json<ClientSnapshot>, ApiError>
{
    app.terminate_targets(form.target_node_id.as_deref(), false).await;
    Ok(Json(snapshot(&*app.inner.read().await)))
}

impl ClientApp {
    pub(super) async fn heartbeat_once(&self) -> anyhow::Result<()> {
        let _heartbeat = self.heartbeat_gate.lock().await;
        self.refresh_auth(false).await?;
        self.refresh_devices().await?;
        self.recover_diagnostic_authorization().await?;
        self.retry_pending_closes().await;
        let (auth, url, identity) = self.auth_context().await.map_err(|error| anyhow::anyhow!(error.message))?;
        let auth_generation = self.inner.read().await.auth_generation;
        let before: HashSet<_> = self.inner.read().await.sessions.keys().cloned().collect();
        let assigned = self.http.get(format!("{url}/api/v1/sessions/assigned/{}", identity.node_id))
            .bearer_auth(auth.access_token).header("x-starbridge-device-token", identity.device_token)
            .send().await?.error_for_status()?.json::<Vec<SessionInfo>>().await?;
        if self.inner.read().await.auth_generation != auth_generation { return Ok(()); }
        let active: HashSet<_> = assigned.iter().map(|session| session.session_id.clone()).collect();
        let stale: Vec<_> = self.inner.read().await.sessions.keys().filter(|id| before.contains(*id) && !active.contains(*id)).cloned().collect();
        for id in stale {
            let mut inner = self.inner.write().await;
            if inner.auth_generation != auth_generation { return Ok(()); }
            let target = inner.sessions.get(&id).map(|session| session.target_node_id.clone());
            if let Some(target) = target {
                if let Some(operation) = inner.operations.get_mut(&target) {
                    if operation.session_id.as_deref() == Some(&id) && !matches!(operation.state.as_str(), "failed" | "terminated") {
                        operation.ssh_ready = false; operation.state = "failed".into();
                        operation.message = "连接已结束或设备离线".into();
                        *inner.connection_generations.entry(target).or_default() += 1;
                    }
                }
            }
            inner.verified_sessions.remove(&id);
            inner.ended_sessions.insert(id.clone());
            drop(inner);
            self.stop_session(&id).await;
        }
        for session in assigned {
            let incoming = session.target_node_id == identity.node_id;
            let (pending, previous, initializing, blocked) = {
                let inner = self.inner.read().await;
                (inner.pending_closes.contains(&session.session_id) || inner.ended_sessions.contains(&session.session_id), inner.sessions.get(&session.session_id).cloned(),
                    inner.route_generations.contains_key(&session.session_id),
                    !incoming && inner.blocked_peers.contains(&session.target_node_id))
            };
            if pending || blocked { self.close_session_local(&session.session_id).await; continue; }
            let running = {
                let mut processes = self.frpc_processes.lock().await;
                processes.get_mut(&session.session_id).is_some_and(|child| child.try_wait().ok().flatten().is_none())
            };
            if incoming {
                let restart = previous.as_ref().is_some_and(|old| old.path != SessionPath::Relay && session.path == SessionPath::Relay);
                if !self.accept_assigned(&session,auth_generation).await { continue; }
                if (!running && !initializing) || restart {
                    self.inner.write().await.route_generations.insert(session.session_id.clone(), 0);
                    let app = self.clone();
                    self.spawn_background(async move { app.prepare_incoming(session, auth_generation).await; });
                } else if !running && initializing && previous.as_ref().is_some_and(|old| old.status == SessionStatus::Connected) {
                    self.inner.write().await.verified_sessions.remove(&session.session_id);
                    self.close_session_local(&session.session_id).await;
                }
            } else if previous.is_some() {
                // Only the outgoing worker can promote readiness; a remote state cannot do so.
                if !self.accept_assigned(&session,auth_generation).await { continue; }
                if !running && self.inner.read().await.verified_sessions.contains_key(&session.session_id) {
                    self.terminate_targets(Some(&session.target_node_id), false).await;
                }
            } else {
                let pending_request = self.inner.read().await.operations.get(&session.target_node_id)
                    .is_some_and(|operation| operation_running(&operation.state));
                if !pending_request { self.close_session_local(&session.session_id).await; }
            }
        }
        let mut inner = self.inner.write().await;
        if inner.auth_generation == auth_generation { inner.stage = "online".into(); }
        Ok(())
    }

    async fn accept_assigned(&self, session:&SessionInfo, auth_generation:u64)->bool {
        let mut inner=self.inner.write().await;
        if inner.auth_generation!=auth_generation || inner.auth.is_none() || inner.closing_account || inner.pending_closes.contains(&session.session_id)
            || inner.ended_sessions.contains(&session.session_id)
            || (session.initiator_node_id==inner.identity.node_id && inner.blocked_peers.contains(&session.target_node_id)) {return false;}
        inner.sessions.insert(session.session_id.clone(),session.clone());
        true
    }

    async fn prepare_incoming(&self, session: SessionInfo, generation: u64) {
        // Admission permits one incoming peer. Serialise preparation and read the latest
        // route after initialization, so an older direct task cannot overwrite a relay route.
        let _incoming = self.incoming_gate.lock().await;
        let result = async {
            self.ensure_ssh_ready().await?;
            self.refresh_devices().await?;
            let latest = {
                let inner = self.inner.read().await;
                if inner.auth_generation != generation || inner.pending_closes.contains(&session.session_id)
                    || !inner.sessions.contains_key(&session.session_id) { anyhow::bail!("连接已终止"); }
                inner.sessions.get(&session.session_id).cloned().expect("active incoming session")
            };
            self.start_frpc_mode(&latest, false, latest.path == SessionPath::Relay).await?;
            let inner = self.inner.read().await;
            if inner.auth_generation != generation || inner.pending_closes.contains(&session.session_id)
                || !inner.sessions.contains_key(&session.session_id) { anyhow::bail!("连接已终止"); }
            Ok::<(), anyhow::Error>(())
        }.await;
        if let Err(error) = result {
            self.stop_session(&session.session_id).await;
            let _ = update_remote_session(self, &session, SessionStatus::Failed, SessionPath::Unknown, Some(error.to_string())).await;
            self.inner.write().await.last_error = Some(format!("接收连接失败：{error}"));
        }
    }

    pub(super) async fn enqueue_connection(&self, target: String, policy: ConnectionPolicy, rebuild: bool, batch_id: Option<&str>)
        -> Result<(), ApiError>
    {
        let refresh_occupancy = {
            let inner = self.inner.read().await;
            inner.auth.is_some() && !inner.closing_account
                && inner.devices.iter().any(|device| device.node_id == target
                    && !device.occupancy.is_empty() && device.occupancy != "idle")
                && !inner.sessions.values().any(|session| session.initiator_node_id == inner.identity.node_id
                    && session.target_node_id == target)
        };
        if refresh_occupancy {
            // A just-closed route can leave the device cache busy until the next heartbeat.
            // Recheck the center before rejecting an immediate reconnect.
            self.refresh_devices().await.map_err(ApiError::internal)?;
        }
        let generation = {
            let mut inner = self.inner.write().await;
            if let Some(batch_id) = batch_id {
                if inner.batch.as_ref().is_none_or(|batch| batch.id != batch_id || batch.cancelled)
                    || inner.blocked_peers.contains(&target) {
                    return Err(ApiError::bad_request("本次任务已终止"));
                }
            }
            if inner.auth.is_none() || inner.closing_account { return Err(ApiError::unauthorized("请先登录")); }
            if self.shutting_down.load(Ordering::Acquire) { return Err(ApiError::bad_request("客户端正在退出")); }
            if target == inner.identity.node_id { return Err(ApiError::bad_request("不能连接本机")); }
            if matches!(inner.self_test.state.as_str(), "running" | "cleaning") {
                return Err(ApiError::bad_request("本机正在进行连接测试"));
            }
            if inner.operations.get(&target).is_some_and(|operation| operation_running(&operation.state) && operation.scheduled) {
                return Ok(());
            }
            if inner.sessions.values().any(|session| session.target_node_id == inner.identity.node_id) {
                return Err(ApiError::bad_request("本机正在被其他设备连接"));
            }
            let device = inner.devices.iter().find(|device| device.node_id == target)
                .ok_or_else(|| ApiError::bad_request("设备不在当前账号中"))?;
            if !device.online { return Err(ApiError::bad_request("设备当前离线，已跳过")); }
            let existing = inner.sessions.values().find(|session| session.initiator_node_id == inner.identity.node_id
                && session.target_node_id == target).cloned();
            if existing.is_none() && device.occupancy != "idle" && !device.occupancy.is_empty() {
                return Err(ApiError::bad_request("设备已被其他连接占用，已跳过"));
            }
            if let Some(session) = &existing {
                if inner.verified_sessions.contains_key(&session.session_id)
                    && !(policy == ConnectionPolicy::DirectOnly && session.path == SessionPath::Relay && rebuild)
                {
                    let retained = policy == ConnectionPolicy::DirectOnly && session.path == SessionPath::Relay;
                    inner.operations.insert(target.clone(), DeviceOperation {
                        node_id: target, state: if retained { "retained_relay" } else { "ready" }.into(),
                        stage: "ready".into(), message: if retained { "已保留现有中转，不计入本次直连成功" } else { "已复用现有连接" }.into(),
                        session_id: Some(session.session_id.clone()), ssh_ready: true, path: session.path,
                        scheduled: true,
                    });
                    return Ok(());
                }
            }
            inner.blocked_peers.remove(&target);
            inner.config.connection_policy = policy;
            save_json(&inner.root.join("config.json"), &inner.config).await.map_err(ApiError::internal)?;
            let generation = inner.connection_generations.entry(target.clone()).or_default();
            *generation += 1;
            let generation = *generation;
            let mut operation = DeviceOperation::queued(target.clone());
            operation.scheduled = true;
            inner.operations.insert(target.clone(), operation);
            generation
        };
        let app = self.clone();
        self.spawn_background(async move { app.connect_worker(target, policy, rebuild, generation).await; });
        Ok(())
    }

    async fn is_current(&self, target: &str, generation: u64) -> bool {
        let inner = self.inner.read().await;
        !self.shutting_down.load(Ordering::Acquire) && inner.auth.is_some() && !inner.closing_account
            && !inner.blocked_peers.contains(target)
            && inner.connection_generations.get(target).copied() == Some(generation)
    }

    async fn progress(&self, target: &str, generation: u64, state: &str, stage: &str, message: &str) {
        let mut inner = self.inner.write().await;
        if inner.connection_generations.get(target).copied() != Some(generation) || inner.blocked_peers.contains(target) { return; }
        if let Some(operation) = inner.operations.get_mut(target) {
            operation.state = state.into(); operation.stage = stage.into(); operation.message = message.into();
        }
    }

    async fn connect_worker(&self, target: String, policy: ConnectionPolicy, rebuild: bool, generation: u64) {
        let Ok(_permit) = self.connection_slots.acquire().await else { return; };
        if !self.is_current(&target, generation).await { return; }
        let result = self.establish_connection(&target, policy, rebuild, generation).await;
        if let Err(error) = result {
            let ids = {
                let mut inner = self.inner.write().await;
                if inner.connection_generations.get(&target).copied()!=Some(generation) || inner.blocked_peers.contains(&target) {return;}
                let ids = inner.sessions.values().filter(|session| session.target_node_id==target
                    && session.initiator_node_id==inner.identity.node_id).map(|session|session.session_id.clone()).collect::<Vec<_>>();
                inner.pending_closes.extend(ids.iter().cloned());inner.ended_sessions.extend(ids.iter().cloned());
                for id in &ids {inner.verified_sessions.remove(id);}
                if let Some(operation)=inner.operations.get_mut(&target) {
                    operation.state="failed".into();operation.message=error.to_string();operation.ssh_ready=false;
                }
                ids
            };
            for id in ids { self.close_session_local(&id).await; }
            self.schedule_close_retry();
        }
    }

    async fn establish_connection(&self, target_id: &str, policy: ConnectionPolicy, rebuild: bool, generation: u64)
        -> anyhow::Result<()>
    {
        self.progress(target_id, generation, "connecting", "components", "正在准备连接组件").await;
        self.ensure_ssh_ready().await?;
        if !self.is_current(target_id, generation).await { anyhow::bail!("连接已终止"); }
        self.refresh_devices().await?;
        let (auth, url, identity) = self.auth_context().await.map_err(|error| anyhow::anyhow!(error.message))?;
        let old = self.inner.read().await.sessions.values().find(|session|
            session.initiator_node_id == identity.node_id && session.target_node_id == target_id).cloned();
        if let Some(old) = &old {
            if rebuild && policy == ConnectionPolicy::DirectOnly && old.path == SessionPath::Relay {
                self.close_session_local(&old.session_id).await;
                self.retry_pending_closes().await;
            }
        }
        if !self.is_current(target_id, generation).await { anyhow::bail!("连接已终止"); }
        self.progress(target_id, generation, "connecting", "contact", "正在联系设备").await;
        let response = self.http.post(format!("{url}/api/v1/sessions"))
            .bearer_auth(&auth.access_token).json(&SessionCreateRequest {
                initiator_node_id: identity.node_id.clone(), device_token: identity.device_token.clone(),
                target_node_id: target_id.into(), policy,
            }).send().await?;
        let mut session: SessionInfo = response_json(response).await.map_err(|error| anyhow::anyhow!(error.message))?;
        let accepted = {
            let mut inner = self.inner.write().await;
            let accepted=inner.connection_generations.get(target_id).copied()==Some(generation)
                && inner.auth.is_some() && !inner.closing_account && !inner.blocked_peers.contains(target_id)
                && !inner.ended_sessions.contains(&session.session_id) && !self.shutting_down.load(Ordering::Acquire);
            if accepted {
                inner.sessions.insert(session.session_id.clone(), session.clone());
                if let Some(operation) = inner.operations.get_mut(target_id) { operation.session_id = Some(session.session_id.clone()); }
            }
            accepted
        };
        if !accepted {
            self.revoke_late_session(&session.session_id, &auth, &url, &identity).await;
            anyhow::bail!("连接已终止；若刚刚终止过旧连接，请等待服务器完成释放后重试");
        }
        // The target can initialise after receiving its incoming negotiating session.
        let mut target = None;
        for _ in 0..20 {
            if !self.is_current(target_id, generation).await { anyhow::bail!("连接已终止"); }
            let devices = self.refresh_devices().await?;
            target = devices.into_iter().find(|device| device.node_id == target_id && !device.ssh_host_key.is_empty());
            if target.is_some() { break; }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        target.ok_or_else(|| anyhow::anyhow!("设备联系失败：目标未完成 SSH 初始化或未发布主机密钥"))?;
        let mut relay = session.policy == ConnectionPolicy::RelayOnly || session.path == SessionPath::Relay;
        self.wait_for_receiver_ready(target_id, &session, generation, relay).await?;
        // The receiver publishes its host key before registering the FRP proxy.
        // Read it again after readiness, rather than pinning a previous session's cached key.
        let mut target = self.refresh_devices().await?.into_iter()
            .find(|device| device.node_id == target_id && device.online && !device.ssh_host_key.is_empty())
            .ok_or_else(|| anyhow::anyhow!("设备联系失败：目标已离线或尚未发布 SSH 主机密钥"))?;
        if !self.is_current(target_id, generation).await { anyhow::bail!("连接已终止"); }
        self.start_frpc_mode(&session, true, relay).await?;
        if !self.is_current(target_id, generation).await { self.close_session_local(&session.session_id).await; anyhow::bail!("连接已终止"); }
        let started = tokio::time::Instant::now();
        let mut last_error = String::new();
        let mut relay_started = None;
        loop {
            if !self.is_current(target_id, generation).await { anyhow::bail!("连接已终止"); }
            let elapsed = started.elapsed().as_secs();
            if elapsed >= 55 {
                let stage = if last_error.contains("Permission denied") || last_error.contains("Host key") { "ssh" }
                    else if relay { "relay" } else { "direct" };
                self.progress(target_id, generation, "failed", stage, "连接验证未通过").await;
                anyhow::bail!("{}：{}", match stage { "ssh" => "SSH 验证失败", "relay" => "中转连接失败", _ => "直连未成功" }, last_error);
            }
            if !relay && elapsed >= 18 {
                if session.policy == ConnectionPolicy::DirectOnly {
                    self.progress(target_id, generation, "failed", "direct", "直连未成功，不会切换中转").await;
                    anyhow::bail!("直连未成功：{last_error}");
                }
                relay = true;
                self.progress(target_id, generation, "connecting", "relay", "直连未成功，正在连接中转").await;
                session = update_remote_session(self, &session, SessionStatus::Negotiating, SessionPath::Relay, None).await?;
                {
                    let mut inner=self.inner.write().await;
                    if inner.connection_generations.get(target_id).copied()!=Some(generation) || inner.blocked_peers.contains(target_id)
                        || inner.ended_sessions.contains(&session.session_id) {anyhow::bail!("连接已终止");}
                    inner.sessions.insert(session.session_id.clone(),session.clone());
                }
                self.wait_for_receiver_ready(target_id, &session, generation, true).await?;
                target = self.refresh_devices().await?.into_iter()
                    .find(|device| device.node_id == target_id && device.online && !device.ssh_host_key.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("设备联系失败：目标已离线或尚未发布 SSH 主机密钥"))?;
                if !self.is_current(target_id, generation).await { anyhow::bail!("连接已终止"); }
                self.start_frpc_mode(&session, true, true).await?;
                relay_started = Some(tokio::time::Instant::now());
            }
            self.progress(target_id, generation, "verifying", if relay { "relay" } else { "direct" },
                if relay { "正在验证中转与 SSH 登录" } else { "正在验证直连与 SSH 登录" }).await;
            match self.verify_session_ssh(&session, &target).await {
                Ok(()) => {
                    if !self.is_current(target_id, generation).await { anyhow::bail!("连接已终止"); }
                    let path = if relay { SessionPath::Relay } else { SessionPath::Direct };
                    let updated = update_remote_session(self, &session, SessionStatus::Connected, path, None).await?;
                    if !self.is_current(target_id, generation).await { anyhow::bail!("连接已终止"); }
                    let mut inner = self.inner.write().await;
                    if inner.connection_generations.get(target_id).copied() != Some(generation) || inner.blocked_peers.contains(target_id) {
                        anyhow::bail!("连接已终止");
                    }
                    inner.sessions.insert(updated.session_id.clone(), updated);
                    inner.verified_sessions.insert(session.session_id.clone(), now_unix());
                    if let Some(operation) = inner.operations.get_mut(target_id) {
                        operation.state = "ready".into(); operation.stage = "ready".into(); operation.ssh_ready = true;
                        operation.path = path; operation.message = "SSH 已验证，可以使用".into();
                    }
                    inner.stage = "online".into();
                    return Ok(());
                }
                Err(error) => {
                    last_error = error.to_string();
                    if last_error.contains("Permission denied") || last_error.contains("REMOTE HOST IDENTIFICATION") {
                        self.progress(target_id, generation, "failed", "ssh", "SSH 验证失败").await;
                        anyhow::bail!("SSH 验证失败：{last_error}");
                    }
                    if relay_started.is_some_and(|time| time.elapsed().as_secs() > 30) {
                        anyhow::bail!("中转连接失败：{last_error}");
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    async fn wait_for_receiver_ready(&self, target_id: &str, session: &SessionInfo, generation: u64, relay: bool)
        -> anyhow::Result<()>
    {
        use std::time::Duration;
        let (url, node_id, auth_generation) = {
            let inner = self.inner.read().await;
            (inner.config.server_url.clone(), inner.identity.node_id.clone(), inner.auth_generation)
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let path = if relay { "relay" } else { "direct" };
        self.progress(target_id, generation, "connecting", "contact", "正在等待目标连接通道就绪").await;
        loop {
            let (auth, token) = {
                let inner = self.inner.read().await;
                self.check_receiver_wait(&inner, target_id, session, generation, auth_generation, &node_id, relay)?;
                (inner.auth.clone().expect("checked login"), inner.identity.device_token.clone())
            };
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() { anyhow::bail!("设备联系失败：目标连接通道尚未就绪，请稍后重试"); }
            let response = tokio::time::timeout(remaining.min(Duration::from_secs(2)), async {
                self.http.get(format!("{url}/api/v1/sessions/{}", session.session_id))
                    .query(&[("ready_path", path)])
                    .bearer_auth(&auth.access_token)
                    .header("x-starbridge-node-id", &node_id)
                    .header("x-starbridge-device-token", token)
                    .send().await?.error_for_status()?.json::<starbridge_protocol::SessionReadiness>().await
            }).await;
            {
                let inner = self.inner.read().await;
                self.check_receiver_wait(&inner, target_id, session, generation, auth_generation, &node_id, relay)?;
            }
            if let Ok(result) = response {
                match result {
                    Ok(result) => {
                        let current = &result.session;
                        let same_route = match (&session.route, &current.route) {
                            (Some(expected), Some(actual)) => expected.server_addr == actual.server_addr
                                && expected.server_port == actual.server_port && expected.proxy_name == actual.proxy_name
                                && expected.secret_key == actual.secret_key && expected.auth_token == actual.auth_token
                                && expected.remote_port == actual.remote_port,
                            _ => false,
                        };
                        if current.session_id != session.session_id || current.initiator_node_id != session.initiator_node_id
                            || current.target_node_id != session.target_node_id || current.policy != session.policy
                            || !matches!(current.status, SessionStatus::Negotiating | SessionStatus::Connected)
                            || (current.policy == ConnectionPolicy::RelayOnly || current.path == SessionPath::Relay) != relay || !same_route {
                            anyhow::bail!("连接已结束或路径已变化，请重新连接");
                        }
                        // Legacy centers return the original SessionInfo without this optional field.
                        // Only their authenticated, still-active session can use the existing behavior.
                        if result.receiver_ready != Some(false) { return Ok(()); }
                    }
                    Err(error) if error.status().is_some_and(|status| status.is_client_error()) => {
                        if error.status() == Some(StatusCode::UNAUTHORIZED)
                            && self.inner.read().await.auth.as_ref().is_some_and(|current| current.access_token != auth.access_token) {
                            // A heartbeat may have rotated this request's token while it was in flight.
                            continue;
                        }
                        anyhow::bail!("设备联系失败：连接会话已失效或验证未通过，请重新连接");
                    }
                    Err(_) => {}
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() { anyhow::bail!("设备联系失败：目标连接通道尚未就绪，请稍后重试"); }
            tokio::time::sleep(remaining.min(Duration::from_millis(250))).await;
        }
    }

    fn check_receiver_wait(&self, inner: &Inner, target_id: &str, session: &SessionInfo,
        generation: u64, auth_generation: u64, node_id: &str, relay: bool) -> anyhow::Result<()>
    {
        if self.shutting_down.load(Ordering::Acquire) || inner.auth.is_none() || inner.closing_account
            || inner.auth_generation != auth_generation || inner.identity.node_id != node_id
            || inner.blocked_peers.contains(target_id) || inner.ended_sessions.contains(&session.session_id)
            || inner.pending_closes.contains(&session.session_id)
            || inner.connection_generations.get(target_id).copied() != Some(generation)
            || !inner.sessions.get(&session.session_id).is_some_and(|current|
                matches!(current.status, SessionStatus::Negotiating | SessionStatus::Connected)
                    && current.initiator_node_id == node_id && current.target_node_id == target_id
                    && (current.policy == ConnectionPolicy::RelayOnly || current.path == SessionPath::Relay) == relay) {
            anyhow::bail!("连接已终止或登录状态已变化");
        }
        Ok(())
    }

    async fn verify_session_ssh(&self, session: &SessionInfo, target: &DeviceInfo) -> anyhow::Result<()> {
        let (root, runtime) = { let inner = self.inner.read().await; (inner.root.clone(), inner.runtime.clone()) };
        let config = std::path::absolute(root.join("ssh/sessions").join(&session.session_id).join("config"))?;
        let marker = format!("STARBRIDGE_{}", uuid::Uuid::new_v4().simple());
        verify_ssh_command(&runtime, &config, &target.ssh_alias, &marker).await
    }

    pub(super) async fn revoke_late_session(&self, id: &str, auth: &AuthResponse, url: &str, identity: &Identity) {
        self.close_session_local(id).await;
        let _ = self.http.delete(format!("{url}/api/v1/sessions/{id}"))
            .bearer_auth(&auth.access_token).header("x-starbridge-node-id", &identity.node_id)
            .header("x-starbridge-device-token", &identity.device_token).send().await;
    }

    pub(super) async fn close_session_local(&self, id: &str) {
        {
            let mut inner = self.inner.write().await;
            inner.pending_closes.insert(id.into());
            inner.ended_sessions.insert(id.into());
            inner.verified_sessions.remove(id);
            let _ = save_json(&inner.root.join("pending-closes.json"), &inner.pending_closes).await;
        }
        self.stop_session(id).await;
    }

    pub(super) fn schedule_close_retry(&self) {
        let app = self.clone();
        self.spawn_background(async move { app.retry_pending_closes().await; });
    }

    pub(super) async fn terminate_targets(&self, target: Option<&str>, include_incoming: bool) {
        let ids = {
            let mut inner = self.inner.write().await;
            let local = inner.identity.node_id.clone();
            let mut peers: HashSet<String> = inner.operations.keys().filter(|id| target.is_none_or(|target| target == id.as_str())).cloned().collect();
            if target.is_none() {
                if let Some(batch) = inner.batch.as_mut() { batch.cancelled = true; peers.extend(batch.targets.iter().cloned()); }
            }
            if let Some(target) = target { peers.insert(target.into()); }
            let ids = inner.sessions.values().filter(|session| {
                if let Some(target) = target {
                    (session.initiator_node_id == local && session.target_node_id == target)
                        || (session.target_node_id == local && session.initiator_node_id == target)
                } else { include_incoming || session.initiator_node_id == local }
            }).map(|session| {
                peers.insert(if session.initiator_node_id == local { session.target_node_id.clone() } else { session.initiator_node_id.clone() });
                session.session_id.clone()
            }).collect::<Vec<_>>();
            for peer in peers {
                *inner.connection_generations.entry(peer.clone()).or_default() += 1;
                inner.blocked_peers.insert(peer.clone());
                if let Some(operation) = inner.operations.get_mut(&peer) {
                    operation.state = "terminated".into(); operation.stage = "terminated".into();
                    operation.message = "连接已终止".into(); operation.ssh_ready = false;
                } else {
                    let mut operation = DeviceOperation::queued(peer.clone());
                    operation.state = "terminated".into(); operation.stage = "terminated".into(); operation.message = "连接已终止".into();
                    inner.operations.insert(peer, operation);
                }
            }
            inner.pending_closes.extend(ids.iter().cloned());
            inner.ended_sessions.extend(ids.iter().cloned());
            for id in &ids { inner.verified_sessions.remove(id); }
            let _ = save_json(&inner.root.join("pending-closes.json"), &inner.pending_closes).await;
            ids
        };
        for id in ids { self.stop_session(&id).await; }
        self.schedule_close_retry();
    }
}

pub(super) async fn verify_ssh_command(runtime: &RuntimeStatus, config: &FsPath, alias: &str, marker: &str)
    -> anyhow::Result<()>
{
    anyhow::ensure!(runtime.ssh_available, "系统 SSH 和包内 SSH 均不可用");
    let result = tokio::time::timeout(std::time::Duration::from_secs(7), async_command(&runtime.ssh_path)
        .args(["-T", "-o", "BatchMode=yes", "-o", "ConnectionAttempts=1", "-o", "ConnectTimeout=4",
            "-o", "ServerAliveInterval=2", "-o", "ServerAliveCountMax=1", "-F"])
        .arg(config).arg(alias).arg(format!("echo {marker}"))
        .stdin(Stdio::null()).output()).await.map_err(|_| anyhow::anyhow!("SSH 登录验证超时"))??;
    if !result.status.success() || !String::from_utf8_lossy(&result.stdout).lines().any(|line| line.trim() == marker) {
        let error = String::from_utf8_lossy(&result.stderr);
        let detail=error.chars().take(500).collect::<String>();
        anyhow::bail!("{}", if detail.trim().is_empty(){"SSH 未正常退出或未返回本次验证内容"}else{detail.trim()});
    }
    Ok(())
}

pub(super) async fn export_connections(State(app): State<ClientApp>, Json(form): Json<ExportForm>)
    -> Result<Json<serde_json::Value>, ApiError>
{
    let inner = app.inner.read().await;
    let ids: Vec<_> = if form.target_node_ids.is_empty() {
        inner.devices.iter().filter(|device| device.node_id != inner.identity.node_id).map(|device| device.node_id.clone()).collect()
    } else { form.target_node_ids };
    let mut items = Vec::new(); let mut excluded = Vec::new(); let mut lines = Vec::new();
    let shell = if cfg!(windows) { "PowerShell" } else { "POSIX Shell" };
    for id in ids {
        let Some(device) = inner.devices.iter().find(|device| device.node_id == id) else { continue; };
        let session = inner.sessions.values().find(|session| session.initiator_node_id == inner.identity.node_id
            && session.target_node_id == id && session.status == SessionStatus::Connected);
        let command = session.and_then(|session| session_ssh_command(&inner, session));
        if let (Some(session), Some(command)) = (session, command) {
            let path = match session.path { SessionPath::Direct => "直连", SessionPath::Relay => "中转", _ => "未确认" };
            let lifetime = "仅适用于发起连接的本机与当前会话；终止、退出或服务重启后请重新获取";
            items.push(serde_json::json!({"name":device.display_name,"purpose":device.purpose,
                "system":format!("{} / {}",device.os,device.arch),"ssh_user":device.ssh_user,
                "privilege":device.ssh_privilege,"path":session.path,"ssh_command":command,
                "source_name":inner.config.display_name,"shell":shell,"lifetime":lifetime}));
            lines.push(format!("设备：{}\n用途：{}\n系统：{} / {}\n登录用户：{}（{}）\n路径：{}\n来源电脑：{}\nShell：{}\nSSH：{}\n有效期：{}",
                device.display_name,if device.purpose.is_empty(){"未设置"}else{&device.purpose},device.os,device.arch,
                device.ssh_user,privilege_label(device.ssh_privilege),path,inner.config.display_name,shell,command,lifetime));
        } else {
            let reason = inner.operations.get(&id).map(|operation| operation.message.as_str()).unwrap_or("当前没有已验证的 SSH 连接");
            excluded.push(serde_json::json!({"name":device.display_name,"reason":reason}));
        }
    }
    let mut text = lines.join("\n\n");
    if !excluded.is_empty() {
        text.push_str("\n\n未包含的设备：\n");
        for item in &excluded { text.push_str(&format!("- {}：{}\n",item["name"].as_str().unwrap_or(""),item["reason"].as_str().unwrap_or(""))); }
    }
    Ok(Json(serde_json::json!({"text":text.trim(),"items":items,"excluded":excluded})))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ReadinessFixture {
        app: ClientApp,
        root: PathBuf,
        session: SessionInfo,
        response: Arc<RwLock<serde_json::Value>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        paths: Arc<RwLock<Vec<String>>>,
        tokens: Arc<RwLock<Vec<String>>>,
        authenticated: Arc<AtomicBool>,
        reject_old: Arc<AtomicBool>,
        hold: Arc<AtomicBool>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        server: tokio::task::JoinHandle<()>,
    }

    impl ReadinessFixture {
        async fn new(ready: Option<bool>) -> Self {
            let root = std::env::temp_dir().join(format!("starbridge-route-ready-{}", uuid::Uuid::new_v4()));
            let app = ClientApp::load(Role::Peer, root.clone()).await.unwrap();
            let identity = app.inner.read().await.identity.clone();
            let session = SessionInfo {
                session_id: "route-ready".into(), initiator_node_id: identity.node_id.clone(), target_node_id: "remote".into(),
                policy: ConnectionPolicy::DirectPreferred, path: SessionPath::Unknown, status: SessionStatus::Negotiating,
                created_at: now_unix(), expires_at: now_unix() + 60, failure_reason: None,
                route: Some(FrpRoute { server_addr: "127.0.0.1".into(), server_port: 17851,
                    auth_token: "fixture".into(), proxy_name: "fixture-route".into(), secret_key: "fixture-key".into(), remote_port: 0 }),
            };
            let mut value = serde_json::to_value(&session).unwrap();
            if let Some(ready) = ready { value["receiver_ready"] = ready.into(); }
            let response = Arc::new(RwLock::new(value));
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let paths = Arc::new(RwLock::new(Vec::new()));
            let tokens = Arc::new(RwLock::new(Vec::new()));
            let authenticated = Arc::new(AtomicBool::new(false));
            let reject_old = Arc::new(AtomicBool::new(false));
            let hold = Arc::new(AtomicBool::new(false));
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let (reply, count, queries, bearers, proof, rejected, paused, started, resume) =
                (response.clone(), calls.clone(), paths.clone(), tokens.clone(), authenticated.clone(), reject_old.clone(), hold.clone(), entered.clone(), release.clone());
            let router = Router::new().route("/api/v1/sessions/{id}", get(move |
                axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>, headers: axum::http::HeaderMap| {
                let (reply, count, queries, bearers, proof, rejected, paused, started, resume, identity) =
                    (reply.clone(), count.clone(), queries.clone(), bearers.clone(), proof.clone(), rejected.clone(), paused.clone(), started.clone(), resume.clone(), identity.clone());
                async move {
                    let value = reply.read().await.clone();
                    let bearer = headers.get("authorization").and_then(|value| value.to_str().ok()).unwrap_or("").to_string();
                    let unauthorized = rejected.load(Ordering::SeqCst) && bearer == "Bearer fixture";
                    count.fetch_add(1, Ordering::SeqCst);
                    queries.write().await.push(query.get("ready_path").cloned().unwrap_or_default());
                    bearers.write().await.push(bearer.clone());
                    proof.store(matches!(bearer.as_str(), "Bearer fixture" | "Bearer fixture-refreshed")
                        && headers.get("x-starbridge-node-id").and_then(|value| value.to_str().ok()) == Some(identity.node_id.as_str())
                        && headers.get("x-starbridge-device-token").and_then(|value| value.to_str().ok()) == Some(identity.device_token.as_str()), Ordering::SeqCst);
                    started.notify_one();
                    if paused.load(Ordering::SeqCst) { resume.notified().await; }
                    if unauthorized { StatusCode::UNAUTHORIZED.into_response() }
                    else if value.is_null() { StatusCode::NOT_FOUND.into_response() } else { Json(value).into_response() }
                }
            }).delete(|| async { StatusCode::NO_CONTENT }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
            {
                let mut inner = app.inner.write().await;
                inner.config.server_url = format!("http://{address}");
                inner.auth = Some(AuthResponse { access_token: "fixture".into(), username: "user".into(),
                    expires_at: u64::MAX, refresh_token: None, refresh_expires_at: None });
                inner.connection_generations.insert("remote".into(), 1);
                inner.sessions.insert(session.session_id.clone(), session.clone());
            }
            Self { app, root, session, response, calls, paths, tokens, authenticated, reject_old, hold, entered, release, server }
        }

        async fn finish(self) {
            self.app.inner.write().await.auth = None;
            self.app.shutdown().await;
            self.server.abort();
            let _ = self.server.await;
            std::fs::remove_dir_all(self.root).unwrap();
        }
    }

    #[tokio::test]
    async fn receiver_readiness_waits_for_registration_before_admitting_first_probe() {
        let fixture = ReadinessFixture::new(Some(false)).await;
        let (app, session) = (fixture.app.clone(), fixture.session.clone());
        let waiting = tokio::spawn(async move { app.wait_for_receiver_ready("remote", &session, 1, false).await });
        fixture.entered.notified().await;
        assert!(!waiting.is_finished(), "An unregistered receiver must not admit SSH or NAT probing");
        fixture.response.write().await["receiver_ready"] = true.into();
        tokio::time::timeout(std::time::Duration::from_secs(2), waiting).await.unwrap().unwrap().unwrap();
        assert!(fixture.calls.load(Ordering::SeqCst) >= 2);
        assert!(fixture.authenticated.load(Ordering::SeqCst));
        assert!(fixture.paths.read().await.iter().all(|path| path == "direct"));
        fixture.finish().await;
    }

    #[tokio::test]
    async fn late_receiver_ready_cannot_resume_a_cancelled_or_changed_account() {
        for change_account in [false, true] {
            let fixture = ReadinessFixture::new(Some(true)).await;
            fixture.hold.store(true, Ordering::SeqCst);
            let (app, session) = (fixture.app.clone(), fixture.session.clone());
            let waiting = tokio::spawn(async move { app.wait_for_receiver_ready("remote", &session, 1, false).await });
            fixture.entered.notified().await;
            if change_account { fixture.app.inner.write().await.auth_generation += 1; }
            else { fixture.app.terminate_targets(Some("remote"), false).await; }
            fixture.release.notify_one();
            assert!(tokio::time::timeout(std::time::Duration::from_secs(2), waiting).await.unwrap().unwrap().is_err());
            fixture.finish().await;
        }
    }

    #[tokio::test]
    async fn receiver_readiness_retries_a_token_rotated_while_the_request_was_in_flight() {
        let fixture = ReadinessFixture::new(Some(true)).await;
        fixture.hold.store(true, Ordering::SeqCst);
        fixture.reject_old.store(true, Ordering::SeqCst);
        let (app, session) = (fixture.app.clone(), fixture.session.clone());
        let waiting = tokio::spawn(async move { app.wait_for_receiver_ready("remote", &session, 1, false).await });
        fixture.entered.notified().await;
        fixture.app.inner.write().await.auth.as_mut().unwrap().access_token = "fixture-refreshed".into();
        fixture.hold.store(false, Ordering::SeqCst);
        fixture.release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), waiting).await.unwrap().unwrap().unwrap();
        assert_eq!(&*fixture.tokens.read().await, &["Bearer fixture", "Bearer fixture-refreshed"]);
        assert!(fixture.authenticated.load(Ordering::SeqCst));
        fixture.finish().await;
    }

    #[tokio::test]
    async fn legacy_readiness_requires_a_valid_active_session_and_matching_route() {
        for invalid in ["none", "closed", "missing", "participant", "route", "path"] {
            let fixture = ReadinessFixture::new(None).await;
            {
                let mut response = fixture.response.write().await;
                match invalid {
                    "closed" => response["status"] = "closed".into(),
                    "missing" => *response = serde_json::Value::Null,
                    "participant" => response["target_node_id"] = "other-device".into(),
                    "route" => response["route"]["proxy_name"] = "old-route".into(),
                    "path" => response["path"] = "relay".into(),
                    _ => {}
                }
            }
            let result = fixture.app.wait_for_receiver_ready("remote", &fixture.session, 1, false).await;
            assert_eq!(result.is_ok(), invalid == "none", "{invalid}");
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
            fixture.finish().await;
        }
    }

    #[tokio::test]
    async fn relay_readiness_queries_the_new_mode_and_rejects_old_direct_evidence() {
        let mut fixture = ReadinessFixture::new(Some(true)).await;
        fixture.session.path = SessionPath::Relay;
        fixture.app.inner.write().await.sessions.get_mut(&fixture.session.session_id).unwrap().path = SessionPath::Relay;
        assert!(fixture.app.wait_for_receiver_ready("remote", &fixture.session, 1, true).await.is_err());
        fixture.response.write().await["path"] = "relay".into();
        fixture.app.wait_for_receiver_ready("remote", &fixture.session, 1, true).await.unwrap();
        assert_eq!(&*fixture.paths.read().await, &["relay", "relay"]);
        fixture.finish().await;
    }

    #[tokio::test]
    async fn relay_only_negotiating_session_accepts_unknown_path_until_verified() {
        let mut fixture = ReadinessFixture::new(Some(true)).await;
        fixture.session.policy = ConnectionPolicy::RelayOnly;
        fixture.app.inner.write().await.sessions.get_mut(&fixture.session.session_id).unwrap().policy = ConnectionPolicy::RelayOnly;
        fixture.response.write().await["policy"] = "relay_only".into();
        fixture.app.wait_for_receiver_ready("remote", &fixture.session, 1, true).await.unwrap();
        assert_eq!(&*fixture.paths.read().await, &["relay"]);
        fixture.finish().await;
    }

    #[tokio::test]
    async fn cached_occupancy_is_rechecked_before_reconnect_admission() {
        for occupancy in ["idle", "incoming"] {
            let root = std::env::temp_dir().join(format!("starbridge-reconnect-{}", uuid::Uuid::new_v4()));
            let app = ClientApp::load(Role::Peer, root.clone()).await.unwrap();
            let mut value = serde_json::to_value(device_registration(&*app.inner.read().await)).unwrap();
            value["node_id"] = "remote".into();
            value["account"] = "user".into();
            value["online"] = true.into();
            value["last_heartbeat"] = now_unix().into();
            value["occupancy"] = occupancy.into();
            let current: DeviceInfo = serde_json::from_value(value).unwrap();
            let mut cached = current.clone();
            cached.occupancy = "incoming".into();
            let local_id = app.inner.read().await.identity.node_id.clone();
            let registry = Router::new().route("/api/v1/devices/enroll", post(move |Json(request): Json<starbridge_protocol::DeviceEnrollmentRequest>| {
                let current = current.clone();
                let node_id = local_id.clone();
                async move { Json(starbridge_protocol::DeviceEnrollmentResponse {
                    registration: Some(DeviceRegistrationResponse { node_id, devices: vec![current] }),
                    device_token: Some(request.registration.device_token), grant: None, candidates: vec![],
                }) }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, registry).await.unwrap(); });
            {
                let mut inner = app.inner.write().await;
                inner.config.server_url = format!("http://{address}");
                inner.auth = Some(AuthResponse { access_token: "test".into(), username: "user".into(),
                    expires_at: u64::MAX, refresh_token: None, refresh_expires_at: None });
                inner.devices = vec![cached];
            }
            // Keep admitted workers queued; this test needs no SSH or FRP processes.
            let _slots = app.connection_slots.acquire_many(3).await.unwrap();
            let result = app.enqueue_connection("remote".into(), ConnectionPolicy::RelayOnly, false, None).await;
            assert_eq!(result.is_ok(), occupancy == "idle");
            assert_eq!(app.inner.read().await.devices[0].occupancy, occupancy);
            app.inner.write().await.auth = None;
            app.shutdown().await;
            server.abort();
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn terminate_all_also_invalidates_targets_not_yet_enqueued() {
        let root = std::env::temp_dir().join(format!("starbridge-batch-{}",uuid::Uuid::new_v4()));
        let app = ClientApp::load(Role::Peer,root.clone()).await.unwrap();
        {
            let mut inner = app.inner.write().await;
            inner.auth = Some(AuthResponse { access_token:"test".into(),username:"user".into(),expires_at:u64::MAX,
                refresh_token:None,refresh_expires_at:None });
            inner.batch = Some(BatchRecord { id:"batch".into(),targets:vec!["not-enqueued".into()],
                policy:ConnectionPolicy::DirectPreferred,cancelled:false });
        }
        app.terminate_targets(None,false).await;
        let result = app.enqueue_connection("not-enqueued".into(),ConnectionPolicy::DirectPreferred,false,Some("batch")).await;
        assert!(result.is_err());
        let inner = app.inner.read().await;
        assert!(inner.blocked_peers.contains("not-enqueued"));
        assert_eq!(inner.operations["not-enqueued"].state,"terminated");
        assert!(!batch_view(&inner).unwrap().running);
        drop(inner);
        app.inner.write().await.auth=None;
        app.shutdown().await; std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn closed_session_tombstone_rejects_late_assignment_after_acknowledgement() {
        let root=std::env::temp_dir().join(format!("starbridge-late-{}",uuid::Uuid::new_v4()));
        let app=ClientApp::load(Role::Peer,root.clone()).await.unwrap();
        let node=app.inner.read().await.identity.node_id.clone();
        let session=SessionInfo {session_id:"closed".into(),initiator_node_id:"remote".into(),target_node_id:node,
            policy:ConnectionPolicy::DirectPreferred,path:SessionPath::Unknown,status:SessionStatus::Negotiating,
            created_at:now_unix(),expires_at:now_unix()+60,failure_reason:None,route:None};
        {
            let mut inner=app.inner.write().await;
            inner.auth=Some(AuthResponse{access_token:"test".into(),username:"user".into(),expires_at:u64::MAX,refresh_token:None,refresh_expires_at:None});
            inner.ended_sessions.insert("closed".into());
            inner.pending_closes.clear();
        }
        assert!(!app.accept_assigned(&session,0).await);
        {
            let mut inner=app.inner.write().await;
            inner.ended_sessions.clear();inner.closing_account=true;
        }
        assert!(!app.accept_assigned(&session,0).await);
        assert!(app.enqueue_connection("remote".into(),ConnectionPolicy::DirectPreferred,false,None).await.is_err());
        app.inner.write().await.auth=None;
        app.shutdown().await;std::fs::remove_dir_all(root).unwrap();
    }
}
