//! A password login can recover this installation; remembered sessions cannot replace it.
use super::*;
use starbridge_protocol::{DeviceEnrollmentRequest, DeviceEnrollmentResponse, EnrollmentCandidate, EnrollmentChoice, new_token};

#[derive(Clone)]
pub(super) struct InstanceLease {
    instance_id: String,
    revision: u64,
}

#[derive(Clone)]
pub(super) struct EnrollmentGrant {
    account: String,
    access_hash: String,
    fingerprint: String,
    expires_at: u64,
    candidates: HashMap<String, String>,
    completed: Option<(EnrollmentChoice, Option<String>, String, String)>,
}

fn instance_id(headers: &HeaderMap) -> &str {
    headers.get("x-starbridge-instance-id").and_then(|value| value.to_str().ok()).unwrap_or("")
}

pub(super) fn check_instance(inner: &Inner, node: &str, headers: &HeaderMap) -> Result<(), ApiError> {
    if inner.instance_leases.get(node).is_some_and(|lease|
        !lease.instance_id.is_empty() && lease.instance_id != instance_id(headers)) {
        return Err(ApiError::forbidden("此设备已由另一实例使用，请重新登录"));
    }
    Ok(())
}

pub(super) fn record_activity(inner: &mut Inner, node: &str, headers: &HeaderMap) {
    let lease = inner.instance_leases.entry(node.into()).or_insert_with(|| InstanceLease {
        instance_id: instance_id(headers).into(), revision: 0,
    });
    lease.revision = lease.revision.wrapping_add(1);
}

pub(super) fn check_legacy_registration(inner: &Inner, account: &str, registration: &DeviceRegistration, headers: &HeaderMap)
    -> Result<(), ApiError>
{
    if let Some((id, _)) = inner.devices.iter().find(|(_, device)| device.account == account
        && (device.registration.node_id == registration.node_id || registration.machine_key.is_some()
            && device.registration.machine_key == registration.machine_key)) {
        check_instance(inner, id, headers)?;
    }
    Ok(())
}

fn fingerprint(request: &DeviceEnrollmentRequest) -> String {
    password_digest(&serde_json::to_string(&(
        &request.registration.node_id, &request.registration.device_token, &request.registration.machine_key,
        &request.instance_id, &request.replacement_token,
    )).expect("serializable enrollment"))
}

fn access_hash(headers: &HeaderMap) -> String {
    password_digest(headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()).unwrap_or(""))
}

fn authorize(inner: &Inner, account: &str, headers: &HeaderMap, request: &DeviceEnrollmentRequest)
    -> Result<Option<EnrollmentGrant>, ApiError>
{
    if let Some(password) = &request.password {
        if !inner.accounts.get(account).is_some_and(|stored| stored.password == password_digest(password)) {
            return Err(ApiError::unauthorized("账户密码不正确，请重新登录"));
        }
        return Ok(Some(EnrollmentGrant {
            account: account.into(), access_hash: access_hash(headers), fingerprint: fingerprint(request),
            expires_at: now_unix() + 300, candidates: HashMap::new(), completed: None,
        }));
    }
    if let Some(token) = &request.grant {
        let grant = inner.enrollment_grants.get(&password_digest(token))
            .filter(|grant| grant.account == account && grant.access_hash == access_hash(headers)
                && grant.fingerprint == fingerprint(request) && grant.expires_at > now_unix())
            .ok_or_else(|| ApiError::unauthorized("本次登录确认已过期，请重新登录"))?;
        return Ok(Some(grant.clone()));
    }
    Ok(None)
}

fn candidates(inner: &Inner, account: &str, registration: &DeviceRegistration) -> Vec<String> {
    let mut ids: Vec<_> = inner.devices.iter().filter(|(_, device)| device.account == account
        && (device.registration.machine_key == registration.machine_key
            || device.registration.machine_key.is_none() && device.registration.node_id == registration.node_id
                && device.device_token == registration.device_token))
        .map(|(id, _)| id.clone()).collect();
    ids.sort();
    ids
}

fn active_connections(inner: &Inner, node: &str) -> usize {
    inner.sessions.values().filter(|session| is_active(session)
        && (session.initiator_node_id == node || session.target_node_id == node)).count()
}

fn candidate_version(inner: &Inner, node: &str) -> String {
    password_digest(&serde_json::to_string(&(
        &inner.devices[node].device_token, active_connections(inner, node), diagnostics::occupied(inner, node),
    )).expect("serializable device state"))
}

fn ready(inner: &Inner, account: &str, node: String, token: String) -> DeviceEnrollmentResponse {
    DeviceEnrollmentResponse {
        registration: Some(DeviceRegistrationResponse { node_id: node, devices: list_devices(inner, Some(account)) }),
        device_token: Some(token), grant: None, candidates: Vec::new(),
    }
}

pub(super) async fn enroll(
    State(state): State<AppState>, headers: HeaderMap, Json(request): Json<DeviceEnrollmentRequest>,
) -> Result<Json<DeviceEnrollmentResponse>, ApiError> {
    let account = authenticate(&state, &headers).await?;
    validate_registration(&request.registration)?;
    if request.registration.machine_key.is_none() || request.instance_id.is_empty() || request.instance_id.len() > 128
        || instance_id(&headers) != request.instance_id
        || request.replacement_token.as_ref().is_some_and(|token| token.len() < 32 || token.len() > 256) {
        return Err(ApiError::bad_request("设备登录数据无效，请重新登录"));
    }
    // Wait for NEW activity instead of treating a stale online timestamp as a live clone.
    // Clients refresh every three seconds; no lock is held while checking liveness.
    let observations = {
        let inner = state.inner.read().await;
        authorize(&inner, &account, &headers, &request)?;
        candidates(&inner, &account, &request.registration).into_iter().filter(|node|
            inner.instance_leases.get(node).is_none_or(|lease| lease.instance_id != request.instance_id)
                && is_online(&inner.devices[node]))
            .map(|node| {
                let revision = inner.instance_leases.get(&node).map(|lease| lease.revision);
                (node, revision)
            }).collect::<Vec<_>>()
    };
    if !observations.is_empty() && request.choice == EnrollmentChoice::Automatic {
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    }
    // Authentication may have expired or been revoked while waiting.
    let account = authenticate(&state, &headers).await?;
    let mut inner = state.inner.write().await;
    let proof = authorize(&inner, &account, &headers, &request)?;
    let fresh = proof.is_some();
    let live: HashSet<_> = observations.into_iter().filter(|(node, revision)| {
        inner.devices.get(node).is_some_and(is_online)
            && inner.instance_leases.get(node).is_some_and(|lease| Some(lease.revision) != *revision)
    }).map(|(node, _)| node).collect();
    let matches = candidates(&inner, &account, &request.registration);
    if let Some(grant) = &proof {
        if let Some((choice, target, node, token)) = &grant.completed {
            if *choice != request.choice || *target != request.target_node_id {
                return Err(ApiError::conflict("本次选择已完成，请刷新设备状态"));
            }
            authenticate_device(&inner, &account, node, token)?;
            check_instance(&inner, node, &headers)?;
            return Ok(Json(ready(&inner, &account, node.clone(), token.clone())));
        }
    }
    let exact = matches.iter().find(|node| {
        let stored = &inner.devices[*node];
        stored.device_token == request.registration.device_token
            || request.replacement_token.as_ref() == Some(&stored.device_token)
    }).cloned();
    let target = match request.choice {
        EnrollmentChoice::Add => {
            if !fresh { return Err(ApiError::forbidden("请重新登录以添加设备")); }
            None
        }
        EnrollmentChoice::Replace => {
            if request.password.is_some() || request.grant.is_none() {
                return Err(ApiError::forbidden("请先查看设备状态，再选择需要替换的实例"));
            }
            let target = request.target_node_id.as_ref().filter(|id| matches.contains(id))
                .ok_or_else(|| ApiError::conflict("设备列表已变化，请重新登录"))?;
            let expected = proof.as_ref().and_then(|grant| grant.candidates.get(target));
            if expected != Some(&candidate_version(&inner, target)) {
                return Err(ApiError::conflict("设备已由其他实例更新，请重新登录"));
            }
            Some(target.clone())
        }
        EnrollmentChoice::Automatic => exact.or_else(|| (matches.len() == 1).then(|| matches[0].clone())),
    };
    let ambiguous = request.choice == EnrollmentChoice::Automatic && target.is_none() && !matches.is_empty();
    let needs_choice = request.choice == EnrollmentChoice::Automatic && (ambiguous || target.as_ref().is_some_and(|node|
        live.contains(node) || inner.instance_leases.get(node).is_none_or(|lease| lease.instance_id != request.instance_id)
            && (active_connections(&inner, node) > 0 || diagnostics::occupied(&inner, node))));
    if needs_choice {
        let mut proof = proof.ok_or_else(|| ApiError::forbidden("另一实例正在使用此设备，请重新登录后选择处理方式"))?;
        let token = new_token();
        let items = matches.iter().map(|node| {
            let device = &inner.devices[node];
            proof.candidates.insert(node.clone(), candidate_version(&inner, node));
            EnrollmentCandidate {
                node_id: node.clone(), display_name: device.registration.display_name.clone(),
                online: live.contains(node), active_connections: active_connections(&inner, node),
                test_running: diagnostics::occupied(&inner, node),
            }
        }).collect();
        inner.enrollment_grants.retain(|_, grant| grant.expires_at > now_unix());
        if inner.enrollment_grants.len() >= 2048 { return Err(ApiError::conflict("登录请求过多，请稍后重试")); }
        inner.enrollment_grants.insert(password_digest(&token), proof);
        return Ok(Json(DeviceEnrollmentResponse { registration: None, device_token: None, grant: Some(token), candidates: items }));
    }
    let previous_devices = inner.devices.clone();
    let previous_deleted = inner.deleted_devices.clone();
    let previous_sessions = inner.sessions.clone();
    let previous_leases = inner.instance_leases.clone();
    let mut registration = request.registration.clone();
    let node = if let Some(node) = target {
        let stored = &inner.devices[&node];
        let same_credential = stored.device_token == registration.device_token
            || request.replacement_token.as_ref() == Some(&stored.device_token);
        if !same_credential && !fresh { return Err(ApiError::forbidden("登录已失效，请重新登录以恢复这台电脑")); }
        if !fresh { ensure_device_not_deleted(&inner, &account, &node, &stored.device_token)?; }
        registration.display_name = stored.registration.display_name.clone();
        registration.purpose = stored.registration.purpose.clone();
        registration.device_token = if request.choice == EnrollmentChoice::Replace || !same_credential {
            request.replacement_token.clone().ok_or_else(|| ApiError::bad_request("缺少本机恢复数据，请重新登录"))?
        } else { stored.device_token.clone() };
        if registration.device_token != stored.device_token {
            let revoked = stored.device_token.clone();
            let machine_key = stored.registration.machine_key.clone();
            let display_name = stored.registration.display_name.clone();
            inner.deleted_devices.entry(node.clone()).or_insert_with(|| DeletedDevice {
                account: account.clone(), revoked_token_hashes: HashSet::new(), machine_key, display_name,
            }).revoked_token_hashes.insert(password_digest(&revoked));
        }
        node
    } else {
        let deleted: Vec<_> = inner.deleted_devices.iter().filter(|(_, record)| record.account == account
            && record.machine_key == registration.machine_key).map(|(id, _)| id.clone()).collect();
        if !fresh && (!deleted.is_empty() || inner.devices.contains_key(&registration.node_id)) {
            return Err(ApiError::forbidden("登录已失效，请重新登录以添加这台电脑"));
        }
        if request.choice == EnrollmentChoice::Add {
            registration.device_token = request.replacement_token.clone()
                .ok_or_else(|| ApiError::bad_request("缺少本机恢复数据，请重新登录"))?;
            new_id()
        } else if deleted.len() == 1 && fresh && !inner.devices.contains_key(&deleted[0]) {
            registration.device_token = request.replacement_token.clone()
                .ok_or_else(|| ApiError::bad_request("缺少本机恢复数据，请重新登录"))?;
            deleted[0].clone()
        } else if inner.devices.contains_key(&registration.node_id)
            || inner.deleted_devices.contains_key(&registration.node_id) { new_id() }
        else { registration.node_id.clone() }
    };
    if request.choice == EnrollmentChoice::Replace {
        for session in inner.sessions.values_mut().filter(|session| is_active(session)
            && (session.initiator_node_id == node || session.target_node_id == node)) {
            session.status = SessionStatus::Closed;
            session.failure_reason = Some("设备已由用户选择的实例接管".into());
        }
    }
    let token = registration.device_token.clone();
    registration.node_id = node.clone();
    let metadata_edited = inner.devices.get(&node).is_some_and(|device| device.metadata_edited);
    inner.devices.insert(node.clone(), StoredDevice {
        account: account.clone(), device_token: token.clone(), registration,
        last_heartbeat: now_unix(), status: "online".into(), metadata_edited,
    });
    let revision = inner.instance_leases.get(&node).map_or(1, |lease| lease.revision.wrapping_add(1));
    inner.instance_leases.insert(node.clone(), InstanceLease { instance_id: request.instance_id.clone(), revision });
    if let Err(error) = persist_inner(&state.data_path, &inner) {
        inner.devices = previous_devices; inner.deleted_devices = previous_deleted;
        inner.sessions = previous_sessions; inner.instance_leases = previous_leases;
        return Err(ApiError::internal(error));
    }
    if request.choice == EnrollmentChoice::Replace { diagnostics::cancel_for_replacement(&mut inner, &node); }
    if let Some(grant) = request.grant.as_ref().and_then(|token| inner.enrollment_grants.get_mut(&password_digest(token))) {
        grant.completed = Some((request.choice, request.target_node_id, node.clone(), token.clone()));
    }
    Ok(Json(ready(&inner, &account, node, token)))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> AppState {
        let root = std::env::temp_dir().join(format!("starbridge-enrollment-{}", new_id()));
        let state = AppState::with_frp_and_data("127.0.0.1".into(), 17851, "test".into(), root);
        {
            let mut inner = state.inner.write().await;
            for name in ["alice", "bob"] {
                inner.accounts.insert(name.into(), Account { password: password_digest("test-password") });
                inner.tokens.insert(name.into(), (name.into(), now_unix() + 3600));
            }
        }
        state
    }

    fn headers(account: &str, instance: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, format!("Bearer {account}").parse().unwrap());
        headers.insert("x-starbridge-instance-id", instance.parse().unwrap());
        headers
    }

    fn request(instance: &str) -> DeviceEnrollmentRequest {
        DeviceEnrollmentRequest {
            registration: serde_json::from_value(serde_json::json!({
                "node_id": new_id(), "device_token": new_token(), "machine_key": "a".repeat(64),
                "display_name": "My computer", "ssh_alias": "test", "ssh_user": "test",
                "capabilities": [], "allow_relay": true, "direct_only": false,
                "version": "test", "os": "windows", "arch": "x86_64", "hostname": "test",
            })).unwrap(),
            instance_id: instance.into(), replacement_token: Some(new_token()),
            password: Some("test-password".into()), grant: None,
            choice: EnrollmentChoice::Automatic, target_node_id: None,
        }
    }

    async fn invoke(state: &AppState, account: &str, request: DeviceEnrollmentRequest)
        -> Result<DeviceEnrollmentResponse, ApiError>
    {
        enroll(State(state.clone()), headers(account, &request.instance_id), Json(request)).await.map(|result| result.0)
    }

    fn resume(mut request: DeviceEnrollmentRequest, result: &DeviceEnrollmentResponse) -> DeviceEnrollmentRequest {
        request.registration.node_id = result.registration.as_ref().unwrap().node_id.clone();
        request.registration.device_token = result.device_token.clone().unwrap();
        request.password = None; request.replacement_token = None;
        request
    }

    async fn cleanup(state: AppState) {
        if state.data_path.exists() { std::fs::remove_dir_all(&*state.data_path).unwrap(); }
    }

    async fn live_challenge(state: &AppState, old: &DeviceEnrollmentRequest) -> (DeviceEnrollmentRequest, DeviceEnrollmentResponse) {
        let state_clone = state.clone();
        let node = old.registration.node_id.clone();
        let old_headers = headers("alice", &old.instance_id);
        let activity = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            record_activity(&mut *state_clone.inner.write().await, &node, &old_headers);
        });
        let mut next = request("replacement");
        let challenge = invoke(state, "alice", next.clone()).await.unwrap();
        activity.await.unwrap();
        assert!(challenge.registration.is_none());
        assert!(challenge.candidates[0].online);
        next.password = None; next.grant = challenge.grant.clone();
        (next, challenge)
    }

    #[tokio::test]
    async fn offline_reinstall_recovers_record_and_revokes_old_credentials() {
        let state = fixture().await;
        let original = request("old");
        let first = invoke(&state, "alice", original.clone()).await.unwrap();
        let old = resume(original, &first);
        state.inner.write().await.devices.get_mut(&old.registration.node_id).unwrap().status = "offline".into();
        let recovered = invoke(&state, "alice", request("fresh")).await.unwrap();
        assert_eq!(first.registration.unwrap().node_id, recovered.registration.unwrap().node_id);
        assert_eq!(invoke(&state, "alice", old).await.err().unwrap().status, StatusCode::FORBIDDEN);
        cleanup(state).await;
    }

    #[tokio::test]
    async fn stale_online_marker_does_not_require_an_instance_choice() {
        let state = fixture().await;
        let first = invoke(&state, "alice", request("stopped")).await.unwrap();
        let recovered = invoke(&state, "alice", request("restored-snapshot")).await.unwrap();
        assert!(recovered.grant.is_none());
        assert_eq!(first.registration.unwrap().node_id, recovered.registration.unwrap().node_id);
        cleanup(state).await;
    }

    #[tokio::test]
    async fn valid_snapshot_can_resume_without_a_password_or_credential_rotation() {
        let state = fixture().await;
        let original = request("before-restart");
        let first = invoke(&state, "alice", original.clone()).await.unwrap();
        let mut restarted = resume(original, &first);
        restarted.instance_id = "after-restart".into();
        let continued = invoke(&state, "alice", restarted).await.unwrap();
        assert_eq!(continued.device_token, first.device_token);
        assert_eq!(continued.registration.unwrap().node_id, first.registration.unwrap().node_id);
        cleanup(state).await;
    }

    #[tokio::test]
    async fn live_clone_can_add_independent_device_and_retry_without_duplicates() {
        let state = fixture().await;
        let original = request("original");
        let first = invoke(&state, "alice", original.clone()).await.unwrap();
        let old = resume(original, &first);
        let (mut next, _) = live_challenge(&state, &old).await;
        next.choice = EnrollmentChoice::Add;
        let added = invoke(&state, "alice", next.clone()).await.unwrap();
        let retry = invoke(&state, "alice", next).await.unwrap();
        assert_eq!(added.registration.as_ref().unwrap().node_id, retry.registration.unwrap().node_id);
        assert_ne!(added.registration.as_ref().unwrap().node_id, old.registration.node_id);
        assert_eq!(state.inner.read().await.devices.len(), 2);
        assert!(invoke(&state, "alice", old).await.is_ok());
        cleanup(state).await;
    }

    #[tokio::test]
    async fn replacement_requires_a_scoped_choice_and_ends_old_connections() {
        let state = fixture().await;
        let original = request("original");
        let first = invoke(&state, "alice", original.clone()).await.unwrap();
        let old = resume(original, &first);
        let node = old.registration.node_id.clone();
        state.inner.write().await.sessions.insert("active".into(), SessionInfo {
            session_id: "active".into(), initiator_node_id: node.clone(), target_node_id: "other".into(),
            policy: ConnectionPolicy::RelayOnly, path: SessionPath::Relay, status: SessionStatus::Connected,
            created_at: now_unix(), expires_at: now_unix() + 600, failure_reason: None, route: None,
        });
        let (mut next, challenge) = live_challenge(&state, &old).await;
        assert_eq!(challenge.candidates[0].active_connections, 1);
        next.choice = EnrollmentChoice::Replace;
        next.target_node_id = Some(node.clone());
        let replaced = invoke(&state, "alice", next).await.unwrap();
        assert_eq!(replaced.registration.unwrap().node_id, node);
        assert_eq!(state.inner.read().await.sessions["active"].status, SessionStatus::Closed);
        assert!(invoke(&state, "alice", old.clone()).await.is_err());
        assert!(check_instance(&*state.inner.read().await, &node, &headers("alice", &old.instance_id)).is_err());
        assert!(challenge.grant.is_some());
        cleanup(state).await;
    }

    #[tokio::test]
    async fn grants_reject_other_accounts_instances_expiry_and_changed_targets() {
        let state = fixture().await;
        let original = request("original");
        let first = invoke(&state, "alice", original.clone()).await.unwrap();
        let old = resume(original, &first);
        let (mut next, _) = live_challenge(&state, &old).await;
        next.choice = EnrollmentChoice::Replace;
        next.target_node_id = Some(old.registration.node_id.clone());
        assert_eq!(invoke(&state, "bob", next.clone()).await.err().unwrap().status, StatusCode::UNAUTHORIZED);
        let mut wrong_instance = next.clone(); wrong_instance.instance_id = "other".into();
        assert_eq!(invoke(&state, "alice", wrong_instance).await.err().unwrap().status, StatusCode::UNAUTHORIZED);
        let hash = password_digest(next.grant.as_ref().unwrap());
        state.inner.write().await.devices.get_mut(&old.registration.node_id).unwrap().device_token = new_token();
        assert_eq!(invoke(&state, "alice", next.clone()).await.err().unwrap().status, StatusCode::CONFLICT);
        state.inner.write().await.enrollment_grants.get_mut(&hash).unwrap().expires_at = 0;
        assert_eq!(invoke(&state, "alice", next).await.err().unwrap().status, StatusCode::UNAUTHORIZED);
        cleanup(state).await;
    }

    #[tokio::test]
    async fn remembered_login_cannot_replace_a_device_and_accounts_stay_separate() {
        let state = fixture().await;
        let first = invoke(&state, "alice", request("alice")).await.unwrap();
        let node = first.registration.unwrap().node_id;
        state.inner.write().await.devices.get_mut(&node).unwrap().status = "offline".into();
        let mut background = request("snapshot"); background.password = None;
        assert_eq!(invoke(&state, "alice", background).await.err().unwrap().status, StatusCode::FORBIDDEN);
        let bob = invoke(&state, "bob", request("bob")).await.unwrap();
        assert_ne!(bob.registration.unwrap().node_id, node);
        assert_eq!(state.inner.read().await.devices.len(), 2);
        cleanup(state).await;
    }

    #[tokio::test]
    async fn failed_persistence_keeps_the_original_identity_and_lease() {
        let state = fixture().await;
        let first = invoke(&state, "alice", request("original")).await.unwrap();
        let node = first.registration.unwrap().node_id;
        state.inner.write().await.devices.get_mut(&node).unwrap().status = "offline".into();
        let original_token = state.inner.read().await.devices[&node].device_token.clone();
        let blocker = state.data_path.join("blocked"); std::fs::write(&blocker, b"file").unwrap();
        let mut broken = state.clone(); broken.data_path = Arc::new(blocker);
        assert!(invoke(&broken, "alice", request("replacement")).await.is_err());
        let inner = state.inner.read().await;
        assert_eq!(inner.devices[&node].device_token, original_token);
        assert!(inner.deleted_devices.is_empty());
        assert!(check_instance(&inner, &node, &headers("alice", "original")).is_ok());
        drop(inner); cleanup(state).await;
    }
}
