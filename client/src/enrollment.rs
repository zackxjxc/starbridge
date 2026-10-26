use super::*;
use starbridge_protocol::{DeviceEnrollmentRequest, DeviceEnrollmentResponse, EnrollmentChoice};

#[derive(Deserialize)]
pub struct ResolveEnrollmentForm {
    pub choice: EnrollmentChoice,
    pub target_node_id: Option<String>,
}

pub(super) async fn resolve_enrollment(
    State(app): State<ClientApp>, Json(form): Json<ResolveEnrollmentForm>,
) -> Result<Json<ClientSnapshot>, ApiError> {
    if form.choice == EnrollmentChoice::Automatic {
        return Err(ApiError::bad_request("请选择新增设备或替换原实例"));
    }
    let _heartbeat = app.heartbeat_gate.lock().await;
    let _gate = app.session_operations.lock().await;
    let _updates = app.device_updates.lock().await;
    let (mut request, generation) = {
        let inner = app.inner.read().await;
        if inner.closing_account { return Err(ApiError::bad_request("正在退出登录")); }
        (inner.pending_enrollment.clone().ok_or_else(|| ApiError::bad_request("登录确认已过期，请重新登录"))?,
            inner.auth_generation)
    };
    request.choice = form.choice;
    request.target_node_id = form.target_node_id;
    app.send_enrollment(request, generation).await.map_err(|error|
        ApiError::new(registration_status(&error).unwrap_or(StatusCode::BAD_GATEWAY), error.to_string()))?;
    Ok(Json(snapshot(&*app.inner.read().await)))
}

impl ClientApp {
    pub(super) async fn enroll_device(&self, password: Option<String>) -> anyhow::Result<Vec<DeviceInfo>> {
        let _updates = self.device_updates.lock().await;
        let (request, generation) = {
            let mut inner = self.inner.write().await;
            if inner.closing_account { anyhow::bail!("正在退出登录"); }
            if password.is_none() && inner.pending_enrollment.is_some() {
                anyhow::bail!("请选择新增设备或替换原实例");
            }
            if password.is_some() {
                let mut identity = inner.identity.clone();
                identity.pending_rebind_token = Some(new_token());
                identity::save_identity(&inner.root, &identity).await?;
                inner.identity = identity;
            }
            let mut registration = device_registration(&inner);
            // The original credential proves continuity; the saved pending token makes
            // recovery retryable if the server commits but its response is lost.
            if password.is_some() { registration.device_token = inner.identity.device_token.clone(); }
            (DeviceEnrollmentRequest {
                registration, instance_id: inner.instance_id.clone(),
                replacement_token: inner.identity.pending_rebind_token.clone(), password,
                grant: None, choice: EnrollmentChoice::Automatic, target_node_id: None,
            }, inner.auth_generation)
        };
        self.send_enrollment(request, generation).await
    }

    async fn send_enrollment(&self, mut request: DeviceEnrollmentRequest, generation: u64)
        -> anyhow::Result<Vec<DeviceInfo>>
    {
        let (auth, url, _) = self.auth_context().await.map_err(|error| anyhow::anyhow!(error.message))?;
        let response = self.http.post(format!("{url}/api/v1/devices/enroll"))
            .bearer_auth(&auth.access_token).json(&request).send().await?;
        let result: DeviceEnrollmentResponse = registration_response(response).await?;
        // Never retain the password in memory across an instance-choice dialog.
        request.password = None;
        if let Some(grant) = result.grant {
            anyhow::ensure!(!result.candidates.is_empty(), "服务器返回的设备选择数据无效");
            request.grant = Some(grant);
            let mut inner = self.inner.write().await;
            anyhow::ensure!(inner.auth_generation == generation && !inner.closing_account, "登录状态已更改");
            inner.pending_enrollment = Some(request);
            inner.enrollment_candidates = result.candidates;
            inner.device_rebind_required = true;
            inner.stage = "device_choice_required".into();
            inner.last_error = None;
            return Ok(inner.devices.clone());
        }
        let registration = result.registration.ok_or_else(|| anyhow::anyhow!("服务器未完成本机登记"))?;
        let token = result.device_token.filter(|token| !token.is_empty())
            .ok_or_else(|| anyhow::anyhow!("服务器未返回本机登录数据"))?;
        {
            let mut inner = self.inner.write().await;
            anyhow::ensure!(inner.auth_generation == generation && !inner.closing_account, "登录状态已更改");
            let mut identity = inner.identity.clone();
            identity.node_id = registration.node_id.clone();
            identity.device_token = token;
            identity.pending_rebind_token = None;
            if identity.node_id != inner.identity.node_id || identity.device_token != inner.identity.device_token
                || inner.identity.pending_rebind_token.is_some() {
                identity::save_identity(&inner.root, &identity).await?;
            }
            inner.identity = identity;
        }
        self.accept_registration(registration, generation).await
    }
}
