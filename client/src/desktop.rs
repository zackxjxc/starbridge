use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{RunEvent, WebviewUrl, WebviewWindowBuilder};

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
async fn client_logout(app: tauri::State<'_, ClientApp>) -> Result<ClientSnapshot, String> {
    Ok(logout(State(app.inner().clone())).await.0)
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
async fn client_copy_nodes(app: tauri::State<'_, ClientApp>) -> Result<serde_json::Value, String> {
    Ok(copy_nodes(State(app.inner().clone())).await.0)
}

/// The event loop stays on the main thread and shares the core's Tokio runtime.
pub fn run(app: ClientApp, role: Role, root: PathBuf) -> anyhow::Result<()> {
    let core = app.clone();
    let desktop = tauri::Builder::default()
        .manage(app)
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(tauri::generate_handler![
            client_state,
            client_config,
            client_save_config,
            client_login,
            client_logout,
            client_connect,
            client_disconnect,
            client_copy_nodes,
        ])
        .build(tauri::generate_context!())?;
    let label = match role {
        Role::Controller => "控制端",
        Role::Executor => "执行端",
    };
    // Build before run so startup failures return to main's error dialog and cleanup.
    WebviewWindowBuilder::new(&desktop, "main", WebviewUrl::App("index.html".into()))
        .title(format!("StarBridge · {label}"))
        .inner_size(1200.0, 820.0)
        .min_inner_size(780.0, 600.0)
        .data_directory(root.join("webview"))
        .on_navigation(|url| {
            (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
                || (matches!(url.scheme(), "http" | "https")
                    && url.host_str() == Some("tauri.localhost"))
        })
        .build()?;
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
