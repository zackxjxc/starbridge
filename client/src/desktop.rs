use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{RunEvent, WebviewUrl, WebviewWindowBuilder};

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
pub fn run(app: ClientApp, _role: Role, root: PathBuf) -> anyhow::Result<()> {
    let core = app.clone();
    let desktop = tauri::Builder::default()
        .manage(app)
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(tauri::generate_handler![
            client_window_action,
            client_state,
            client_config,
            client_save_config,
            client_login,
            client_register,
            client_update_device,
            client_delete_device,
            client_connect_all,
            client_terminate,
            client_export,
            client_start_test,
            client_cancel_test,
            client_components,
            client_logout,
            client_connect,
            client_disconnect,
            client_disconnect_session,
            client_copy_nodes,
        ])
        .build(tauri::generate_context!())?;
    // Build before run so startup failures return to main's error dialog and cleanup.
    let window = WebviewWindowBuilder::new(&desktop, "main", WebviewUrl::App("index.html".into()))
        .title("StarBridge")
        .inner_size(900.0, 600.0)
        .min_inner_size(900.0, 600.0)
        .max_inner_size(900.0, 600.0)
        .resizable(false)
        .maximizable(false)
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
    window.build()?;
    let closing = AtomicBool::new(false);
    desktop.run(move |handle, event| {
        if let RunEvent::ExitRequested { api, .. } = event {
            if !closing.swap(true, Ordering::SeqCst) {
                api.prevent_exit();
                let core = core.clone();
                let handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    core.shutdown().await;
                    handle.exit(0);
                });
            }
        }
    });
    Ok(())
}
