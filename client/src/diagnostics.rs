use super::*;
use sha2::{Digest, Sha256};
use starbridge_protocol::{DiagnosticCreateRequest, DiagnosticSession, DiagnosticUpdateRequest};

struct DiagnosticContext {
    generation: u64, url: String, identity: Identity,
    cleanup_access: std::sync::Mutex<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TestStep { pub id: String, pub label: String, pub state: String, pub message: String }

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TestView {
    pub id: Option<String>, pub state: String, pub stage: String, pub message: String,
    pub steps: Vec<TestStep>, pub started_at: Option<u64>, pub finished_at: Option<u64>,
}

impl Default for TestView {
    fn default() -> Self {
        Self { id: None, state: "idle".into(), stage: "idle".into(), message: "未进行连接测试".into(),
            steps: [ ("components", "连接组件"), ("relay", "服务器中转"), ("ssh", "SSH 发起"),
                ("sshd", "SSHD 接收"), ("cleanup", "资源清理"), ("sftp", "文件传输") ]
                .into_iter().map(|(id,label)| TestStep { id:id.into(), label:label.into(),
                    state:if id=="sftp" {"skipped"}else{"pending"}.into(),
                    message:if id=="sftp" {"本次未测试文件传输"}else{"尚未测试"}.into() }).collect(),
            started_at: None, finished_at: None }
    }
}

pub(super) async fn start_test(State(app): State<ClientApp>) -> Result<Json<ClientSnapshot>, ApiError> {
    let _gate=app.diagnostic_gate.lock().await;
    let generation = {
        let mut inner = app.inner.write().await;
        if inner.auth.is_none() || inner.closing_account { return Err(ApiError::unauthorized("请先登录")); }
        if matches!(inner.self_test.state.as_str(), "running" | "cleaning") { return Ok(Json(snapshot(&inner))); }
        if !inner.sessions.is_empty() || inner.operations.values().any(|operation|
            matches!(operation.state.as_str(), "queued" | "connecting" | "verifying")) {
            return Err(ApiError::bad_request("请先终止现有连接和等待任务，再进行本机测试"));
        }
        inner.self_test = TestView::default();
        if !inner.config.allow_relay || inner.config.direct_only {
            inner.self_test.state = "unverified".into();
            inner.self_test.message = "当前策略不允许中转测试，尚未验证".into();
            return Ok(Json(snapshot(&inner)));
        }
        if std::env::var_os("STARBRIDGE_SSH_PORT").is_some() {
            inner.self_test.state = "unverified".into();
            inner.self_test.message = "外部 SSH 服务未提供受限测试授权，接收能力尚未验证".into();
            return Ok(Json(snapshot(&inner)));
        }
        inner.self_test.state = "running".into(); inner.self_test.stage = "contact".into();
        app.diagnostic_cancel.store(false, Ordering::Release);
        inner.self_test.message = "正在申请连接测试".into(); inner.self_test.started_at = Some(now_unix());
        save_json(&inner.root.join("last-test.json"), &inner.self_test).await.map_err(ApiError::internal)?;
        inner.auth_generation
    };
    let worker = app.clone();
    app.spawn_background(async move { worker.run_diagnostic(generation).await; });
    Ok(Json(snapshot(&*app.inner.read().await)))
}

pub(super) async fn cancel_test(State(app): State<ClientApp>) -> Result<Json<ClientSnapshot>, ApiError> {
    app.diagnostic_cancel.store(true, Ordering::Release);
    let mut inner = app.inner.write().await;
    if inner.self_test.state == "running" {
        inner.self_test.message = "正在取消测试并清理临时连接".into();
    }
    Ok(Json(snapshot(&inner)))
}

impl ClientApp {
    async fn diagnostic_context(&self, generation: Option<u64>) -> anyhow::Result<DiagnosticContext> {
        let inner = self.inner.read().await;
        if inner.auth.is_none() || inner.closing_account { anyhow::bail!("请先登录"); }
        if generation.is_some_and(|generation| generation != inner.auth_generation) {
            anyhow::bail!("登录状态已更改");
        }
        Ok(DiagnosticContext { generation: inner.auth_generation,
            url: inner.config.server_url.clone(), identity: inner.identity.clone(),
            cleanup_access: std::sync::Mutex::new(inner.auth.as_ref().expect("authenticated context").access_token.clone()) })
    }

    async fn diagnostic_request<F>(&self, context: &DiagnosticContext, cleanup_only: bool, request: F)
        -> anyhow::Result<reqwest::Response>
    where F: Fn(&str) -> reqwest::RequestBuilder
    {
        for attempt in 0..2 {
            let (token, can_refresh) = {
                let inner = self.inner.read().await;
                if inner.auth_generation != context.generation || inner.closing_account
                    || inner.config.server_url != context.url || inner.auth.is_none() {
                    if !cleanup_only { anyhow::bail!("登录状态已更改"); }
                    // Logout retains the old grant briefly for this task's bounded cleanup.
                    // Never borrow the next account's credentials or refresh an old grant.
                    (context.cleanup_access.lock().expect("diagnostic token lock").clone(), false)
                } else {
                    let token = inner.auth.as_ref().expect("authenticated context").access_token.clone();
                    *context.cleanup_access.lock().expect("diagnostic token lock") = token.clone();
                    (token, true)
                }
            };
            if !cleanup_only && self.test_cancelled() { anyhow::bail!("连接测试已取消"); }
            let response = request(&token).send().await?;
            // Only a rejected credential is safe to retry, especially for lease creation.
            if response.status() != StatusCode::UNAUTHORIZED || attempt == 1 || !can_refresh { return Ok(response); }
            if !cleanup_only && self.test_cancelled() { anyhow::bail!("连接测试已取消"); }
            if cleanup_only {
                let inner = self.inner.read().await;
                if inner.auth_generation != context.generation || inner.closing_account { return Ok(response); }
            }
            self.refresh_auth_matching(true, Some((context.generation, &token))).await?;
        }
        unreachable!("diagnostic request has at most one authentication retry")
    }

    async fn test_step(&self, id: &str, state: &str, message: &str) {
        let mut inner = self.inner.write().await;
        inner.self_test.stage = id.into(); inner.self_test.message = message.into();
        if let Some(step) = inner.self_test.steps.iter_mut().find(|step| step.id == id) {
            step.state = state.into(); step.message = message.into();
        }
    }

    fn test_cancelled(&self) -> bool {
        self.diagnostic_cancel.load(Ordering::Acquire) || self.shutting_down.load(Ordering::Acquire)
    }

    async fn run_diagnostic(&self, generation: u64) {
        let mut processes: Vec<tokio::process::Child> = Vec::new();
        let mut directory = None;
        let mut diagnostic: Option<DiagnosticSession> = None;
        let context = self.diagnostic_context(Some(generation)).await;
        // Keep the bounded create request alive after Cancel so an accepted reservation
        // is identified and revoked. Dropping this response can leave an unknown lease.
        let reservation = match &context {
            Ok(context) => self.request_diagnostic(context).await,
            Err(error) => Err(anyhow::anyhow!(error.to_string())),
        };
        let result = match reservation {
            Ok(diag) => {
                diagnostic=Some(diag.clone());
                if self.test_cancelled() { Err(anyhow::anyhow!("连接测试已取消")) }
                else if let Ok(context)=&context {
                    let cancelled = async {
                        loop { if self.test_cancelled() { return; }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    };
                    tokio::select! {
                        result = tokio::time::timeout(std::time::Duration::from_secs(150),
                            self.diagnostic_sequence(context,&diag,&mut directory,&mut processes)) =>
                            match result { Ok(result) => result, Err(_) => Err(anyhow::anyhow!("连接测试超时")) },
                        _ = cancelled => Err(anyhow::anyhow!("连接测试已取消")),
                    }
                } else {Err(anyhow::anyhow!("登录状态已失效"))}
            }
            Err(error)=>Err(error),
        };
        let cancelled = self.test_cancelled();
        let mut remote_cleanup_uncertain = false;
        // Recover an accepted create whose HTTP response was cancelled or lost.
        if diagnostic.is_none() && (cancelled || result.is_err()) {
            if let Ok(context) = &context {
                let DiagnosticContext { url, identity, .. } = context;
                let request = self.diagnostic_request(context, true, |token| self.http
                    .delete(format!("{url}/api/v1/diagnostics/current/{}", identity.node_id))
                    .bearer_auth(token).header("x-starbridge-node-id",&identity.node_id)
                    .header("x-starbridge-device-token",&identity.device_token));
                match tokio::time::timeout(std::time::Duration::from_secs(4), request).await {
                    Ok(Ok(response)) if response.status().is_success() => {
                        diagnostic = response.json::<Option<DiagnosticSession>>().await.ok().flatten();
                        if diagnostic.is_none() && !result.as_ref().is_err_and(|error|error.to_string().contains("不支持")
                            || error.to_string().contains("404") || error.to_string().contains("503")) {
                            remote_cleanup_uncertain=true;
                        }
                    }
                    Ok(Ok(response)) if response.status()==StatusCode::NOT_FOUND || response.status()==StatusCode::SERVICE_UNAVAILABLE => {}
                    _ => remote_cleanup_uncertain = true,
                }
            }
        }
        let local_relay = diagnostic.as_ref().is_some_and(|diagnostic| diagnostic.local_relay);
        let unsupported = diagnostic.is_none() && result.as_ref().is_err_and(|error|
            error.to_string().contains("不支持") || error.to_string().contains("404") || error.to_string().contains("503"));
        {
            let mut inner = self.inner.write().await;
            inner.self_test.state = "cleaning".into();
        }
        self.test_step("cleanup", "running", "正在关闭测试连接与临时授权").await;
        // Revoke remotely before local cleanup when possible, but never wait for the network to stop children.
        let mut cleanup_error = remote_cleanup_uncertain.then(|| "远程诊断申请的撤销状态未确认；授权将按期限回收".to_string());
        for child in &mut processes {
            if let Err(error) = child.kill().await {
                if child.try_wait().ok().flatten().is_none() { cleanup_error = Some(format!("测试进程清理失败：{error}")); }
            }
            let _ = child.wait().await;
        }
        if let Some(dir) = &directory {
            if let Err(error) = tokio::fs::remove_dir_all(dir).await { cleanup_error = Some(format!("测试目录清理失败：{error}")); }
        }
        if let (Some(diag), Ok(context)) = (&diagnostic, &context) {
            let DiagnosticContext { url, identity, .. } = context;
            let outcome = if cancelled { "cancelled" } else if result.is_ok() && cleanup_error.is_none() { "succeeded" } else { "failed" };
            let report = self.diagnostic_request(context, true, |token| self.http.post(format!("{url}/api/v1/diagnostics/{}/status", diag.test_id))
                .bearer_auth(token).header("x-starbridge-node-id", &identity.node_id)
                .header("x-starbridge-device-token", &identity.device_token)
                .json(&DiagnosticUpdateRequest { phase:"cleanup".into(), ssh_verified: result.is_ok(),
                    cleanup_complete:cleanup_error.is_none(), outcome:Some(outcome.into()),
                    failure_reason:result.as_ref().err().map(ToString::to_string) }));
            match tokio::time::timeout(std::time::Duration::from_secs(5), report).await {
                Ok(Ok(response)) if response.status().is_success() => {
                    if let Ok(mut remote) = response.json::<DiagnosticSession>().await {
                        for _ in 0..5 {
                            if remote.cleanup_complete { break; }
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            let request = self.diagnostic_request(context, true, |token| self.http.get(format!("{url}/api/v1/diagnostics/{}", diag.test_id))
                                .bearer_auth(token).header("x-starbridge-node-id",&identity.node_id)
                                .header("x-starbridge-device-token",&identity.device_token));
                            if let Ok(Ok(response)) = tokio::time::timeout(std::time::Duration::from_secs(2), request).await {
                                if let Ok(updated) = response.json::<DiagnosticSession>().await { remote = updated; }
                            }
                        }
                        if !remote.cleanup_complete { cleanup_error = Some("本地已清理，服务器尚未确认测试路由关闭".into()); }
                    } else {
                        cleanup_error = Some("本地已清理，服务器清理确认返回无效".into());
                    }
                }
                _ => cleanup_error = Some("本地已清理，服务器清理确认未完成；授权将按期限回收".into()),
            }
        }
        let mut inner = self.inner.write().await;
        let cleanup_ok = cleanup_error.is_none();
        if let Some(step) = inner.self_test.steps.iter_mut().find(|step| step.id == "cleanup") {
            step.state = if cleanup_ok {"passed"}else{"failed"}.into();
            step.message = cleanup_error.clone().unwrap_or_else(|| "测试进程、临时密钥和配置已清理".into());
        }
        inner.self_test.state = if !cleanup_ok { "failed" } else if cancelled { "cancelled" }
            else if unsupported || (local_relay && result.is_ok()) { "unverified" }
            else if result.is_ok() && cleanup_ok { "passed" } else { "failed" }.into();
        inner.self_test.message = if let Some(error) = cleanup_error {
            result.as_ref().err().map(|cause|format!("{cause}；{error}")).unwrap_or(error)
        }
            else if cancelled { "未完成连接测试：本次测试已取消".into() }
            else if local_relay && result.is_ok() { "本地联调通过；远程中转环境尚未验证".into() }
            else { match result { Ok(()) => "本机连接测试通过：SSH 发起、SSHD 接收、服务器中转均正常。跨设备直连将在实际连接时检查。".into(), Err(error) => error.to_string() } };
        let final_message = inner.self_test.message.clone();
        for step in &mut inner.self_test.steps {
            if matches!(step.state.as_str(), "pending" | "running") {
                step.state = if cancelled || unsupported {"unverified"}else{"failed"}.into();
                step.message = format!("未完成：{final_message}");
            }
        }
        inner.self_test.finished_at = Some(now_unix());
        let _ = save_json(&inner.root.join("last-test.json"), &inner.self_test).await;
        if cleanup_ok { let _ = tokio::fs::remove_file(inner.root.join("active-test.json")).await; }
    }

    async fn request_diagnostic(&self,context:&DiagnosticContext)->anyhow::Result<DiagnosticSession> {
        let DiagnosticContext { url, identity, .. } = context;
        let root=self.inner.read().await.root.clone();
        save_json(&root.join("active-test.json"),&serde_json::json!({"request_pending":true,"created_at":now_unix()})).await?;
        let response=self.diagnostic_request(context, false, |token| self.http.post(format!("{url}/api/v1/diagnostics"))
            .bearer_auth(token).json(&DiagnosticCreateRequest {
                node_id:identity.node_id.clone(),device_token:identity.device_token.clone() })).await?;
        let diag:DiagnosticSession=response_json(response).await.map_err(|error|anyhow::anyhow!(error.message))?;
        if !diag.test_id.chars().all(|c|c.is_ascii_alphanumeric()||c=='-')
            || !diag.challenge.chars().all(|c|c.is_ascii_alphanumeric()||c=='-'||c=='_') {anyhow::bail!("服务器返回无效的测试标识");}
        self.inner.write().await.self_test.id=Some(diag.test_id.clone());
        save_json(&root.join("active-test.json"),&serde_json::json!({"test_id":diag.test_id,"created_at":now_unix()})).await?;
        Ok(diag)
    }

    pub(super) async fn recover_diagnostic_authorization(&self) -> anyhow::Result<()> {
        // A recovery request must never revoke a new test started during its HTTP wait.
        let _gate=self.diagnostic_gate.lock().await;
        let root={
            let inner=self.inner.read().await;
            if matches!(inner.self_test.state.as_str(),"running"|"cleaning") {return Ok(());}
            inner.root.clone()
        };
        let marker=match tokio::fs::read(root.join("active-test.json")).await {
            Ok(bytes)=>serde_json::from_slice::<serde_json::Value>(&bytes)?,
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return Ok(()),
            Err(error)=>return Err(error.into()),
        };
        let context=self.diagnostic_context(None).await?;
        let DiagnosticContext { url, identity, .. } = &context;
        let response=self.diagnostic_request(&context, true, |token| self.http.delete(format!("{url}/api/v1/diagnostics/current/{}",identity.node_id))
            .bearer_auth(token).header("x-starbridge-node-id",&identity.node_id)
            .header("x-starbridge-device-token",&identity.device_token)).await?.error_for_status()?;
        let remote=response.json::<Option<DiagnosticSession>>().await?;
        let clear=if let Some(diag)=remote {
            let local_clean=recover_test_files(&root).await;
            let response=self.diagnostic_request(&context, true, |token| self.http.post(format!("{url}/api/v1/diagnostics/{}/status",diag.test_id))
                .bearer_auth(token).header("x-starbridge-node-id",&identity.node_id)
                .header("x-starbridge-device-token",&identity.device_token)
                .json(&DiagnosticUpdateRequest{phase:"cleanup".into(),ssh_verified:false,cleanup_complete:local_clean,
                    outcome:Some("cancelled".into()),failure_reason:Some("恢复清理上次未完成的测试".into())})
                ).await?.error_for_status()?;
            response.json::<DiagnosticSession>().await?.cleanup_complete && local_clean
        } else {
            // A previous HTTP create may still be completing on the server. Keep checking
            // beyond its bounded probe/request window before considering an empty result final.
            now_unix().saturating_sub(marker["created_at"].as_u64().unwrap_or(0))>=20
        };
        if clear {
            tokio::fs::remove_file(root.join("active-test.json")).await?;
            let mut inner=self.inner.write().await;
            if let Some(step)=inner.self_test.steps.iter_mut().find(|step|step.id=="cleanup") {
                step.state="passed".into();step.message="上次测试的临时资源已恢复清理".into();
            }
            save_json(&inner.root.join("last-test.json"),&inner.self_test).await?;
        }
        Ok(())
    }

    async fn diagnostic_sequence(&self, context: &DiagnosticContext,
        diag: &DiagnosticSession, directory: &mut Option<PathBuf>,
        processes: &mut Vec<tokio::process::Child>) -> anyhow::Result<()>
    {
        let DiagnosticContext { url, identity, .. } = context;
        let (root,runtime) = { let inner = self.inner.read().await; (inner.root.clone(),inner.runtime.clone()) };
        self.test_step("components", "running", "检查正式连接组件").await;
        for name in ["frpc","ssh","ssh-keygen","sshd","sshd-session","sshd-auth","sftp-server"] {
            let path = runtime_file(&runtime,name);
            if !path.is_file() { anyhow::bail!("连接组件缺失：{}",path.display()); }
        }
        let dir = std::path::absolute(root.join("diagnostics").join(&diag.test_id))?;
        tokio::fs::create_dir_all(&dir).await?;
        *directory = Some(dir.clone());
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&dir,std::fs::Permissions::from_mode(0o700)).await?; }
        let public = ensure_key(&runtime_file(&runtime,"ssh-keygen"),&dir.join("id_ed25519")).await?;
        let host = ensure_key(&runtime_file(&runtime,"ssh-keygen"),&dir.join("host_ed25519")).await?;
        tokio::fs::write(dir.join("authorized_keys"),format!("restrict {public}\n")).await?;
        let ssh_port = pick_port(24000,24999)?;
        let visitor_port = pick_port(25000,25999)?;
        let config = format!("{}ForceCommand echo {}\nAllowTcpForwarding no\nAllowAgentForwarding no\nPermitTTY no\nX11Forwarding no\nPermitTunnel no\n",
            sshd_config(&dir,&std::path::absolute(runtime_file(&runtime,"sftp-server"))?,ssh_port),diag.challenge);
        let sshd_config_path = dir.join("sshd_config");
        tokio::fs::write(&sshd_config_path,config).await?;
        processes.push(spawn_test_process(&std::path::absolute(runtime_file(&runtime,"sshd"))?,
            &["-D".into(),"-e".into(),"-f".into(),sshd_config_path.to_string_lossy().into_owned()],&dir.join("sshd.log"))?);
        self.process_job.attach(processes.last().expect("registered test child"))?;
        let session = SessionInfo { session_id:diag.test_id.clone(),initiator_node_id:identity.node_id.clone(),
            target_node_id:identity.node_id.clone(),policy:ConnectionPolicy::RelayOnly,path:SessionPath::Relay,
            status:SessionStatus::Negotiating,created_at:diag.created_at,expires_at:diag.expires_at,failure_reason:None,route:Some(diag.route.clone()) };
        let provider = frpc_config(&diag.route,&session,false,ssh_port,true);
        let visitor = frpc_config(&diag.route,&session,true,visitor_port,true);
        let metadata = format!("metadatas.starbridge_test_id = \"{}\"\nmetadatas.starbridge_test_token = \"{}\"\n",diag.test_id,diag.route.secret_key);
        for (name,config) in [("provider",provider),("visitor",visitor)] {
            let path = dir.join(format!("{name}.toml"));
            tokio::fs::write(&path,format!("{metadata}{config}")).await?;
            processes.push(spawn_test_process(FsPath::new(&runtime.frpc_path),
                &["-c".into(),path.to_string_lossy().into_owned()],&dir.join(format!("{name}.log")))?);
            self.process_job.attach(processes.last().expect("registered test child"))?;
        }
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        for process in processes.iter_mut() {
            if let Some(status) = process.try_wait()? { anyhow::bail!("测试组件启动后退出：{status}；请查看安全软件记录与组件来源"); }
        }
        self.test_step("components","passed","正式连接组件已启动").await;
        self.test_step("relay","running","正在建立服务器中转路径").await;
        let client_config = dir.join("config");
        tokio::fs::write(&client_config,format!("Host diagnostic\n HostName 127.0.0.1\n Port {visitor_port}\n User {}\n IdentityFile {}\n IdentitiesOnly yes\n HostKeyAlias diagnostic\n UserKnownHostsFile {}\n StrictHostKeyChecking yes\n",
            current_user(),ssh_path(&dir.join("id_ed25519")),ssh_path(&dir.join("known_hosts")))).await?;
        tokio::fs::write(dir.join("known_hosts"),format!("diagnostic {host}\n")).await?;
        let mut last_error = None;
        for _ in 0..8 {
            match verify_ssh_command(&runtime,&client_config,"diagnostic",&diag.challenge).await {
                Ok(()) => { last_error = None; break; }
                Err(error) => { last_error = Some(error); tokio::time::sleep(std::time::Duration::from_secs(1)).await; }
            }
        }
        if let Some(error) = last_error { anyhow::bail!("SSH 验证失败：{error}"); }
        self.test_step("ssh","passed","SSH 已校验主机身份、完成公钥登录并收到本次挑战值").await;
        self.test_step("sshd","passed","受限 SSHD 已接收连接并完成固定内容回传").await;
        let response = self.diagnostic_request(context, false, |token| self.http.post(format!("{url}/api/v1/diagnostics/{}/status",diag.test_id))
            .bearer_auth(token).header("x-starbridge-node-id",&identity.node_id)
            .header("x-starbridge-device-token",&identity.device_token)
            .json(&DiagnosticUpdateRequest {phase:"verify".into(),ssh_verified:true,cleanup_complete:false,outcome:None,failure_reason:None})
            ).await?.error_for_status()?;
        let mut remote: DiagnosticSession = response.json().await?;
        for _ in 0..15 {
            if remote.relay_verified || remote.local_relay { break; }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            remote = self.diagnostic_request(context, false, |token| self.http.get(format!("{url}/api/v1/diagnostics/{}",diag.test_id))
                .bearer_auth(token).header("x-starbridge-node-id",&identity.node_id)
                .header("x-starbridge-device-token",&identity.device_token)
                ).await?.error_for_status()?.json().await?;
        }
        if remote.local_relay {
            self.test_step("relay","unverified","本地中转联调通过；远程中转环境未验证").await;
        } else if remote.relay_verified {
            self.test_step("relay","passed","服务器已确认本次 STCP 路由的数据转发").await;
        } else { anyhow::bail!("SSH 已通过，但服务器中转数据证据未确认，验证未完成"); }
        Ok(())
    }
}

fn spawn_test_process(path: &FsPath, args: &[String], log: &FsPath) -> anyhow::Result<tokio::process::Child> {
    let output = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    Ok(async_command(path).args(args).stdin(Stdio::null()).stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output)).spawn()?)
}

pub(super) async fn recover_test_files(root: &FsPath) -> bool {
    // All children of this fixed app-owned directory are diagnostic scratch data.
    let directory = root.join("diagnostics");
    let mut entries=match tokio::fs::read_dir(&directory).await {
        Ok(entries)=>entries,
        Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return true,
        Err(_)=>return false,
    };
    let mut clean=true;
    loop {
        match entries.next_entry().await {
            Ok(Some(entry))=>match entry.file_type().await {
                Ok(kind) if kind.is_dir() && !kind.is_symlink()=> {
                    if tokio::fs::remove_dir_all(entry.path()).await.is_err(){clean=false;}
                }
                _=>clean=false,
            },
            Ok(None)=>break,
            Err(_)=>{clean=false;break;}
        }
    }
    clean
}

pub(super) async fn components(State(app): State<ClientApp>) -> Result<Json<serde_json::Value>, ApiError> {
    let runtime = app.inner.read().await.runtime.clone();
    let root = FsPath::new(&runtime.frpc_path).parent().unwrap_or(FsPath::new(""));
    let manifest: serde_json::Value = tokio::fs::read(root.join("runtime-manifest.json")).await.ok()
        .and_then(|bytes|serde_json::from_slice(&bytes).ok()).unwrap_or(serde_json::json!([]));
    let mut files = Vec::new();
    for name in ["frpc","ssh","sshd","ssh-keygen","sshd-session","sshd-auth","sftp-server"] {
        let path = runtime_file(&runtime,name);
        let filename = path.file_name().unwrap_or_default().to_string_lossy().to_string();
        let actual = tokio::fs::read(&path).await.ok().map(|bytes|format!("{:x}",Sha256::digest(bytes)));
        let record = manifest.as_array().and_then(|records| records.iter().find(|record|
            record["files"].as_array().is_some_and(|files|files.iter().any(|file|file.as_str()==Some(&filename)))));
        let expected = record.and_then(|record| record["file_sha256"][&filename].as_str());
        let (verification,message) = match (actual.as_deref(),expected) {
            (None,_) => ("missing","文件缺失"),
            (Some(actual),Some(expected)) if actual.eq_ignore_ascii_case(expected) => ("verified","与发布清单中的文件 SHA-256 一致"),
            (Some(_),Some(_)) => ("mismatch","文件与发布清单不一致，请重新获取完整发布包"),
            _ => ("not_verified","已计算当前文件 SHA-256；缺少逐文件发布校验值，尚未验证来源一致性"),
        };
        files.push(serde_json::json!({"name":filename,"path":path.to_string_lossy(),"sha256":actual,
            "verification":verification,"message":message,"version":record.and_then(|record|record["version"].as_str()),
            "source_url":record.and_then(|record|record["url"].as_str())}));
    }
    Ok(Json(serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"official_url":"https://github.com/fatedier/frp/releases",
        "checksums_url":"https://github.com/fatedier/frp/releases","windows_help_url":"https://support.microsoft.com/en-us/windows/security/threat-malware-protection/virus-and-threat-protection-in-the-windows-security-app",
        "message":"FRP 使用官方发布组件，打包时不修改程序本体。文件校验只说明与发布清单一致。", "files":files})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct MockServer {
        app: ClientApp,
        mode: &'static str,
        calls: Arc<Mutex<Vec<(String, String)>>>,
    }

    fn auth(token: &str) -> AuthResponse {
        AuthResponse { access_token: token.into(), username: "test-user".into(), expires_at: u64::MAX,
            refresh_token: Some("test-refresh".into()), refresh_expires_at: Some(u64::MAX) }
    }

    fn diagnostic() -> DiagnosticSession {
        DiagnosticSession { test_id: "test-lease".into(), node_id: "test-node".into(), status: "running".into(),
            phase: "contact".into(), created_at: now_unix(), expires_at: now_unix()+60, challenge: "test-challenge".into(),
            route: FrpRoute { server_addr: "127.0.0.1".into(), server_port: 1, auth_token: "test".into(),
                proxy_name: "test".into(), secret_key: "test".into(), remote_port: 1 },
            relay_verified: false, ssh_verified: false, cleanup_complete: true, local_relay: false, failure_reason: None }
    }

    async fn respond(State(mock): State<MockServer>, request: axum::extract::Request) -> Response {
        let path = request.uri().path().to_string();
        let token = request.headers().get("authorization").and_then(|value| value.to_str().ok()).unwrap_or("").to_string();
        mock.calls.lock().await.push((path.clone(), token.clone()));
        if path == "/api/v1/auth/refresh" {
            if mock.mode == "change-during-refresh" {
                let mut inner = mock.app.inner.write().await;
                inner.auth_generation += 1;
                inner.auth = Some(auth("other-account"));
            }
            return Json(auth("new-access")).into_response();
        }
        if mock.mode == "cancel-accepted" {
            mock.app.diagnostic_cancel.store(true, Ordering::Release);
            return Json(diagnostic()).into_response();
        }
        if matches!(mock.mode, "logout-accepted" | "account-accepted") {
            if path == "/api/v1/diagnostics" {
                mock.app.diagnostic_cancel.store(true, Ordering::Release);
                let mut inner = mock.app.inner.write().await;
                inner.auth_generation += 1;
                if mock.mode == "logout-accepted" { inner.closing_account = true; }
                else {
                    inner.auth = Some(auth("other-account"));
                    inner.config.server_url = "http://127.0.0.1:1".into();
                    inner.identity.node_id = "other-node".into();
                    inner.identity.device_token = "other-device".into();
                }
            } else {
                assert_eq!(token, "Bearer old-access");
                assert_eq!(request.headers()["x-starbridge-node-id"], "test-node");
                assert_eq!(request.headers()["x-starbridge-device-token"], "test-device");
            }
            return Json(diagnostic()).into_response();
        }
        if mock.mode == "ordinary-error" { return StatusCode::INTERNAL_SERVER_ERROR.into_response(); }
        if token == "Bearer old-access" || mock.mode == "always-unauthorized" {
            match mock.mode {
                "cancel" => mock.app.diagnostic_cancel.store(true, Ordering::Release),
                "logout" | "account-change" => {
                    let mut inner = mock.app.inner.write().await;
                    inner.auth_generation += 1;
                    inner.auth = if mock.mode == "logout" { None } else { Some(auth("other-account")) };
                }
                _ => {}
            }
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Json(diagnostic()).into_response()
    }

    async fn fixture(mode: &'static str) -> (ClientApp, PathBuf, MockServer, tokio::task::JoinHandle<()>) {
        let root = std::env::temp_dir().join(format!("starbridge-diagnostic-auth-{}", uuid::Uuid::new_v4()));
        let app = ClientApp::load(Role::Peer, root.clone()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        {
            let mut inner = app.inner.write().await;
            inner.config.server_url = format!("http://{}", listener.local_addr().unwrap());
            inner.config.remember_login = false;
            inner.auth = Some(auth("old-access"));
            inner.identity.node_id = "test-node".into();
            inner.identity.device_token = "test-device".into();
        }
        let mock = MockServer { app: app.clone(), mode, calls: Arc::new(Mutex::new(Vec::new())) };
        let router = Router::new().fallback(respond).with_state(mock.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        (app, root, mock, server)
    }

    async fn finish(app: ClientApp, root: PathBuf, server: tokio::task::JoinHandle<()>) {
        app.inner.write().await.auth = None;
        app.shutdown().await;
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn expired_access_refreshes_creation_and_later_requests_use_latest_token() {
        let (app, root, mock, server) = fixture("refresh").await;
        let context = app.diagnostic_context(None).await.unwrap();
        assert_eq!(app.request_diagnostic(&context).await.unwrap().test_id, "test-lease");
        for method in [reqwest::Method::GET, reqwest::Method::POST, reqwest::Method::DELETE] {
            app.diagnostic_request(&context, true, |token| app.http.request(method.clone(),
                format!("{}/api/v1/diagnostics/test-lease", context.url)).bearer_auth(token))
                .await.unwrap().error_for_status().unwrap();
        }
        let calls = mock.calls.lock().await;
        assert_eq!(calls.len(), 6);
        assert_eq!(calls[0].1, "Bearer old-access");
        assert_eq!(calls[1].0, "/api/v1/auth/refresh");
        assert!(calls[2..].iter().all(|(_, token)| token == "Bearer new-access"));
        drop(calls);
        finish(app, root, server).await;
    }

    #[tokio::test]
    async fn diagnostic_authentication_retry_is_bounded_and_ordinary_errors_are_not_retried() {
        for (mode, expected_calls, expected_status) in [
            ("ordinary-error", 1, StatusCode::INTERNAL_SERVER_ERROR),
            ("always-unauthorized", 3, StatusCode::UNAUTHORIZED),
        ] {
            let (app, root, mock, server) = fixture(mode).await;
            let context = app.diagnostic_context(None).await.unwrap();
            let response = app.diagnostic_request(&context, true, |token|
                app.http.get(format!("{}/api/v1/diagnostics/test-lease", context.url)).bearer_auth(token)).await.unwrap();
            assert_eq!(response.status(), expected_status);
            assert_eq!(mock.calls.lock().await.len(), expected_calls);
            finish(app, root, server).await;
        }
    }

    #[tokio::test]
    async fn cancelled_or_changed_account_cannot_retry_rejected_creation() {
        for mode in ["cancel", "logout", "account-change", "change-during-refresh"] {
            let (app, root, mock, server) = fixture(mode).await;
            let context = app.diagnostic_context(None).await.unwrap();
            assert!(app.request_diagnostic(&context).await.is_err());
            let calls = mock.calls.lock().await;
            assert_eq!(calls.iter().filter(|(path, _)| path == "/api/v1/diagnostics").count(), 1, "{mode}");
            assert_eq!(calls.len(), if mode == "change-during-refresh" { 2 } else { 1 }, "{mode}");
            drop(calls);
            if mode == "change-during-refresh" || mode == "account-change" {
                assert_eq!(app.inner.read().await.auth.as_ref().unwrap().access_token, "other-account");
            }
            finish(app, root, server).await;
        }
    }

    #[tokio::test]
    async fn cancellation_keeps_accepted_lease_available_for_cleanup() {
        let (app, root, mock, server) = fixture("cancel-accepted").await;
        app.run_diagnostic(0).await;
        let inner = app.inner.read().await;
        assert_eq!(inner.self_test.id.as_deref(), Some("test-lease"));
        assert_eq!(inner.self_test.state, "cancelled");
        assert!(!root.join("active-test.json").exists());
        drop(inner);
        let calls = mock.calls.lock().await;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "/api/v1/diagnostics/test-lease/status");
        drop(calls);
        finish(app, root, server).await;
    }

    #[tokio::test]
    async fn logout_or_account_change_cleans_accepted_lease_only_with_original_credentials() {
        for mode in ["logout-accepted", "account-accepted"] {
            let (app, root, mock, server) = fixture(mode).await;
            // Logout holds this gate while waiting for the diagnostic worker to finish.
            let account_gate = app.session_operations.lock().await;
            tokio::time::timeout(std::time::Duration::from_secs(2), app.run_diagnostic(0))
                .await.expect("cleanup must not wait for the logout authentication gate");
            drop(account_gate);
            assert_eq!(app.inner.read().await.self_test.state, "cancelled");
            assert!(!root.join("active-test.json").exists());
            let calls = mock.calls.lock().await;
            assert_eq!(calls.len(), 2, "{mode}");
            assert_eq!(calls[1].0, "/api/v1/diagnostics/test-lease/status");
            assert_eq!(calls[1].1, "Bearer old-access");
            drop(calls);
            finish(app, root, server).await;
        }
    }
}
