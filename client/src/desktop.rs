use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

#[derive(Default)]
struct DesktopLifecycle {
    exit_pending: AtomicBool,
    shutdown_complete: AtomicBool,
    english: AtomicBool,
}

struct ExitMenu(MenuItem<tauri::Wry>);

#[tauri::command]
async fn client_version() -> HealthResponse {
    health().await.0
}

#[tauri::command]
async fn client_startup_status(startup: tauri::State<'_, crate::startup::ProfileStartup>) -> Result<Option<crate::profile::StartupIssue>, String> {
    Ok(startup.status().await)
}

#[tauri::command]
async fn client_startup_retry(handle: tauri::AppHandle, startup: tauri::State<'_, crate::startup::ProfileStartup>) -> Result<(), String> {
    let core = startup.retry().await?;
    if handle.manage(core.clone()) { core.heartbeat_loop().await; }
    Ok(())
}

#[tauri::command]
async fn client_startup_recover(handle: tauri::AppHandle, startup: tauri::State<'_, crate::startup::ProfileStartup>, form: LoginForm) -> Result<(), String> {
    let core = startup.recover_account(form).await?;
    if handle.manage(core.clone()) { core.heartbeat_loop().await; }
    Ok(())
}

struct DesktopText {
    exit: &'static str,
    exiting: &'static str,
    tooltip: &'static str,
    confirm_title: &'static str,
    confirm_message: &'static str,
    cancel: &'static str,
}

fn desktop_text(english: bool) -> DesktopText {
    if english {
        DesktopText {
            exit: "Exit", exiting: "Exiting…", tooltip: "StarBridge · Running in background",
            confirm_title: "Exit StarBridge?",
            confirm_message: "Exiting will disconnect all connections on this computer, cancel queued tasks and connection tests, and stop background services.",
            cancel: "Cancel",
        }
    } else {
        DesktopText {
            exit: "退出", exiting: "正在退出…", tooltip: "StarBridge · 后台运行",
            confirm_title: "退出 StarBridge？",
            confirm_message: "退出将断开本机所有连接，取消等待任务和连接测试，并停止后台服务。",
            cancel: "取消",
        }
    }
}

fn current_text(handle: &tauri::AppHandle) -> DesktopText {
    desktop_text(handle.state::<DesktopLifecycle>().english.load(Ordering::Acquire))
}

#[tauri::command]
fn client_set_desktop_locale(handle: tauri::AppHandle, window: tauri::WebviewWindow, locale: String) -> Result<(), String> {
    if window.label() != "main" {
        return Err("Only the main window can update the desktop language".into());
    }
    let english = match locale.as_str() {
        "en" => true,
        "zh-CN" => false,
        _ => return Err("Unsupported desktop language".into()),
    };
    handle.state::<DesktopLifecycle>().english.store(english, Ordering::Release);
    let text = desktop_text(english);
    if let Some(item) = handle.try_state::<ExitMenu>() {
        let exiting = !item.0.is_enabled().map_err(|error| error.to_string())?;
        item.0.set_text(if exiting { text.exiting } else { text.exit }).map_err(|error| error.to_string())?;
    }
    if let Some(tray) = handle.tray_by_id("main") {
        tray.set_tooltip(Some(text.tooltip)).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn restore_main(handle: &tauri::AppHandle) -> tauri::Result<()> {
    if let Some(window) = handle.get_webview_window("main") {
        window.show()?;
        window.unminimize()?;
        window.set_focus()?;
    }
    Ok(())
}

fn start_shutdown(handle: tauri::AppHandle) {
    handle.state::<crate::startup::ProfileStartup>().begin_shutdown();
    if let Some(item) = handle.try_state::<ExitMenu>() {
        let _ = item.0.set_enabled(false);
        let _ = item.0.set_text(current_text(&handle).exiting);
    }
    tauri::async_runtime::spawn(async move {
        handle.state::<crate::startup::ProfileStartup>().shutdown().await;
        handle.state::<DesktopLifecycle>().shutdown_complete.store(true, Ordering::Release);
        handle.exit(0);
    });
}

fn requires_exit_confirmation<'a>(
    mut sessions: impl Iterator<Item = SessionStatus>,
    mut operations: impl Iterator<Item = &'a str>,
    test_state: &str,
) -> bool {
    sessions.any(|status| matches!(status, SessionStatus::Negotiating | SessionStatus::Connected))
        || operations.any(|state| matches!(state, "queued" | "connecting" | "verifying"))
        || matches!(test_state, "running" | "cleaning")
}

fn request_exit(handle: &tauri::AppHandle) {
    // Reserve the whole check / confirmation / cleanup flow, including repeated OS exit requests.
    if handle.state::<DesktopLifecycle>().exit_pending.swap(true, Ordering::AcqRel) {
        return;
    }
    let handle = handle.clone();
    tauri::async_runtime::spawn(async move {
        let active = {
            if let Some(core) = handle.try_state::<ClientApp>() {
                let inner = core.inner.read().await;
                requires_exit_confirmation(
                    inner.sessions.values().map(|session| session.status),
                    inner.operations.values().map(|operation| operation.state.as_str()),
                    &inner.self_test.state,
                )
            } else { false }
        };
        if !active {
            start_shutdown(handle);
            return;
        }
        let _ = restore_main(&handle);
        let text = current_text(&handle);
        let mut dialog = handle.dialog()
            .message(text.confirm_message)
            .title(text.confirm_title)
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(text.exit.into(), text.cancel.into()));
        if let Some(window) = handle.get_webview_window("main") {
            dialog = dialog.parent(&window);
        }
        dialog.show(move |confirmed| {
            if confirmed {
                start_shutdown(handle);
            } else {
                handle.state::<DesktopLifecycle>().exit_pending.store(false, Ordering::Release);
            }
        });
    });
}

fn create_tray(handle: &tauri::AppHandle) -> anyhow::Result<()> {
    let text = current_text(handle);
    let exit = MenuItem::with_id(handle, "exit", text.exit, true, None::<&str>)?;
    let menu = Menu::with_items(handle, &[&exit])?;
    let icon = handle.default_window_icon().cloned()
        .ok_or_else(|| anyhow::anyhow!("缺少应用图标"))?;
    let _tray = TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip(text.tooltip)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|handle, event| {
            if event.id.as_ref() == "exit" {
                request_exit(handle);
            }
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(event, TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }) {
                if let Err(error) = restore_main(tray.app_handle()) {
                    tracing::warn!(%error, "无法从托盘恢复窗口");
                }
            }
        })
        .build(handle)?;
    #[cfg(windows)]
    if _tray.rect()?.is_none() {
        handle.remove_tray_by_id("main");
        anyhow::bail!("系统托盘区域不可用");
    }
    handle.manage(ExitMenu(exit));
    Ok(())
}

#[tauri::command]
fn client_window_action(window: tauri::WebviewWindow, action: String) -> Result<(), String> {
    match action.as_str() {
        "drag" => window.start_dragging(),
        "minimize" => window.minimize(),
        "close" => window.close(),
        _ => return Err("不支持的窗口操作".into()),
    }
    .map_err(|error| error.to_string())
}

#[tauri::command]
async fn client_state(app: tauri::State<'_, ClientApp>) -> Result<ClientSnapshot, String> {
    Ok(state(State(app.inner().clone())).await.0)
}

#[tauri::command]
async fn client_config(app: tauri::State<'_, ClientApp>) -> Result<ClientConfig, String> {
    Ok(get_config(State(app.inner().clone())).await.0)
}

#[tauri::command]
async fn client_save_config(
    app: tauri::State<'_, ClientApp>,
    config: ClientConfig,
) -> Result<ClientConfig, String> {
    update_config(State(app.inner().clone()), Json(config))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_login(
    app: tauri::State<'_, ClientApp>,
    form: LoginForm,
) -> Result<ClientSnapshot, String> {
    login(State(app.inner().clone()), Json(form))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_rebind_device(
    app: tauri::State<'_, ClientApp>,
    form: DeviceRebindForm,
) -> Result<ClientSnapshot, String> {
    rebind_device(State(app.inner().clone()), Json(form))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_resolve_enrollment(
    app: tauri::State<'_, ClientApp>, form: enrollment::ResolveEnrollmentForm,
) -> Result<ClientSnapshot, String> {
    enrollment::resolve_enrollment(State(app.inner().clone()), Json(form)).await
        .map(|value| value.0).map_err(|error| error.message)
}

#[tauri::command]
async fn client_register(
    app: tauri::State<'_, ClientApp>,
    form: RegisterForm,
) -> Result<ClientSnapshot, String> {
    register(State(app.inner().clone()), Json(form))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_update_device(
    app: tauri::State<'_, ClientApp>,
    form: DeviceMetadataForm,
) -> Result<ClientSnapshot, String> {
    update_device(State(app.inner().clone()), Json(form))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_delete_device(
    app: tauri::State<'_, ClientApp>,
    form: DeviceDeleteForm,
) -> Result<ClientSnapshot, String> {
    delete_device(State(app.inner().clone()), Json(form))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_connect_all(
    app: tauri::State<'_, ClientApp>,
    form: BatchConnectForm,
) -> Result<ClientSnapshot, String> {
    connect_all(State(app.inner().clone()), Json(form))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_terminate(
    app: tauri::State<'_, ClientApp>,
    form: TerminateForm,
) -> Result<ClientSnapshot, String> {
    terminate(State(app.inner().clone()), Json(form))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_export(
    app: tauri::State<'_, ClientApp>,
    form: ExportForm,
) -> Result<serde_json::Value, String> {
    export_connections(State(app.inner().clone()), Json(form))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_start_test(app: tauri::State<'_, ClientApp>) -> Result<ClientSnapshot, String> {
    start_test(State(app.inner().clone()))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_cancel_test(app: tauri::State<'_, ClientApp>) -> Result<ClientSnapshot, String> {
    cancel_test(State(app.inner().clone()))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_components(app: tauri::State<'_, ClientApp>) -> Result<serde_json::Value, String> {
    components(State(app.inner().clone()))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_connection_history(
    app: tauri::State<'_, ClientApp>,
    form: SessionHistoryQuery,
) -> Result<SessionHistoryPage, String> {
    connection_history(State(app.inner().clone()), Json(form))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_logout(app: tauri::State<'_, ClientApp>) -> Result<ClientSnapshot, String> {
    logout(State(app.inner().clone()))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_connect(
    app: tauri::State<'_, ClientApp>,
    form: ConnectForm,
) -> Result<ClientSnapshot, String> {
    connect(State(app.inner().clone()), Json(form))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_disconnect(app: tauri::State<'_, ClientApp>) -> Result<ClientSnapshot, String> {
    disconnect(State(app.inner().clone()))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_disconnect_session(
    app: tauri::State<'_, ClientApp>,
    session_id: String,
) -> Result<ClientSnapshot, String> {
    disconnect_session(State(app.inner().clone()), axum::extract::Path(session_id))
        .await
        .map(|value| value.0)
        .map_err(|error| error.message)
}

#[tauri::command]
async fn client_copy_nodes(app: tauri::State<'_, ClientApp>) -> Result<serde_json::Value, String> {
    Ok(copy_nodes(State(app.inner().clone())).await.0)
}

/// The event loop stays on the main thread and shares the core's Tokio runtime.
pub fn run(loaded: anyhow::Result<ClientApp>, role: Role, root: PathBuf) -> anyhow::Result<()> {
    let initial_core = loaded.as_ref().ok().cloned();
    let builder = tauri::Builder::default()
        .manage(crate::startup::ProfileStartup::new(role, root.clone(), loaded))
        .manage(DesktopLifecycle::default())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            client_version,
            client_startup_status,
            client_startup_retry,
            client_startup_recover,
            client_set_desktop_locale,
            client_window_action,
            client_state,
            client_config,
            client_save_config,
            client_login,
            client_register,
            client_rebind_device,
            client_resolve_enrollment,
            client_update_device,
            client_delete_device,
            client_connect_all,
            client_terminate,
            client_export,
            client_start_test,
            client_cancel_test,
            client_components,
            client_connection_history,
            client_logout,
            client_connect,
            client_disconnect,
            client_disconnect_session,
            client_copy_nodes,
        ]);
    let builder = if let Some(core) = initial_core { builder.manage(core) } else { builder };
    let desktop = builder.build(tauri::generate_context!())?;
    // Linux does not deliver tray left-click events, so this one-item menu cannot restore a window there.
    let tray_available = if cfg!(target_os = "linux") {
        false
    } else {
        match create_tray(desktop.handle()) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(%error, "托盘创建失败，关闭窗口将退出客户端");
                false
            }
        }
    };
    // Build before run so startup failures return to main's error dialog and cleanup.
    let window = WebviewWindowBuilder::new(&desktop, "main", WebviewUrl::App("index.html".into()))
        .title("StarBridge")
        .inner_size(900.0, 600.0)
        .min_inner_size(900.0, 600.0)
        .max_inner_size(900.0, 600.0)
        .resizable(false)
        .maximizable(false)
        .initialization_script(format!("Object.defineProperty(window, '__STARBRIDGE_TRAY__', {{ value: {tray_available} }});"))
        .data_directory(root.join("webview"))
        .on_navigation(|url| {
            (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
                || (matches!(url.scheme(), "http" | "https")
                    && url.host_str() == Some("tauri.localhost"))
        });
    #[cfg(windows)]
    let window = window
        .decorations(false)
        .shadow(true)
        .initialization_script("Object.defineProperty(window, '__STARBRIDGE_FRAMELESS__', { value: true });");
    #[cfg(target_os = "macos")]
    let window = window
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(tauri::LogicalPosition::new(12.0, 22.0))
        .initialization_script("Object.defineProperty(window, '__STARBRIDGE_MACOS_OVERLAY__', { value: true });");
    window.build()?;
    desktop.run(move |handle, event| {
        match event {
            RunEvent::WindowEvent { label, event: WindowEvent::CloseRequested { api, .. }, .. }
                if label == "main" => {
                api.prevent_close();
                if handle.state::<DesktopLifecycle>().exit_pending.load(Ordering::Acquire) {
                    return;
                }
                if tray_available && handle.try_state::<ClientApp>().is_some() && handle.tray_by_id("main").is_some() {
                    if let Some(window) = handle.get_webview_window("main") {
                        if let Err(error) = window.hide() {
                            tracing::warn!(%error, "隐藏窗口失败，改为正常退出");
                            request_exit(handle);
                        }
                    }
                } else {
                    request_exit(handle);
                }
            }
            RunEvent::ExitRequested { api, .. } => {
                if handle.state::<DesktopLifecycle>().shutdown_complete.load(Ordering::Acquire) {
                    return;
                }
                api.prevent_exit();
                request_exit(handle);
            }
            _ => {}
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_copy_covers_both_languages() {
        let english = desktop_text(true);
        let chinese = desktop_text(false);
        assert_eq!(english.exit, "Exit");
        assert_eq!(chinese.exit, "退出");
        for (en, zh) in [
            (english.exiting, chinese.exiting), (english.tooltip, chinese.tooltip),
            (english.confirm_title, chinese.confirm_title), (english.confirm_message, chinese.confirm_message),
            (english.cancel, chinese.cancel),
        ] {
            assert!(!en.is_empty());
            assert_ne!(en, zh);
            assert!(!en.chars().any(|c| ('\u{3400}'..='\u{9fff}').contains(&c)));
        }
    }

    #[test]
    fn exit_confirms_any_active_session_including_incoming() {
        for status in [SessionStatus::Negotiating, SessionStatus::Connected] {
            assert!(requires_exit_confirmation(
                [SessionStatus::Closed, status].into_iter(), std::iter::empty(), "idle",
            ));
        }
        assert!(!requires_exit_confirmation(
            [SessionStatus::Closed, SessionStatus::Failed].into_iter(), std::iter::empty(), "idle",
        ));
    }

    #[test]
    fn exit_confirms_queued_connections_and_test_cleanup() {
        for state in ["queued", "connecting", "verifying"] {
            assert!(requires_exit_confirmation(std::iter::empty(), ["failed", state].into_iter(), "idle"));
        }
        for state in ["running", "cleaning"] {
            assert!(requires_exit_confirmation(std::iter::empty(), std::iter::empty(), state));
        }
    }

    #[test]
    fn exit_does_not_confirm_idle_or_finished_work() {
        for state in ["idle", "passed", "failed", "cancelled", "unverified"] {
            assert!(!requires_exit_confirmation(
                std::iter::empty(), ["failed", "terminated", "skipped"].into_iter(), state,
            ));
        }
    }
}
