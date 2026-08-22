//! Commands backing the networked-inference settings panel: switching between
//! local and client mode, running the server, and probing a remote one.

use std::net::IpAddr;

use log::info;
use serde::Serialize;
use specta::Type;
use tauri::AppHandle;

use crate::remote_client::{self, RemoteModel, RemoteServerInfo};
use crate::server;
use crate::settings::{self, InferenceMode, SecretString};

/// Everything the settings panel needs to describe the local server's state.
#[derive(Debug, Clone, Serialize, Type)]
pub struct ServerStatus {
    pub running: bool,
    /// The address actually bound, e.g. `0.0.0.0:8756`. `None` when stopped.
    pub bound_address: Option<String>,
    /// LAN addresses a client on another machine can use, with the port already
    /// appended — the value the user copies into the client's settings.
    pub client_urls: Vec<String>,
    pub token: String,
}

#[tauri::command]
#[specta::specta]
pub fn change_inference_mode(app: AppHandle, mode: String) -> Result<(), String> {
    let parsed = match mode.as_str() {
        "local" => InferenceMode::Local,
        "client" => InferenceMode::Client,
        other => return Err(format!("Unknown inference mode '{other}'")),
    };

    let mut current = settings::get_settings(&app);
    current.inference_mode = parsed;
    settings::write_settings(&app, current);
    info!("Inference mode set to {parsed:?}");
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_server_enabled(app: AppHandle, enabled: bool) -> Result<(), String> {
    if enabled {
        // Generating the token before persisting the flag keeps the invariant
        // that an enabled server always has one — `apply_server_settings`
        // refuses to bind otherwise.
        server::ensure_server_token(&app);
    }

    let mut current = settings::get_settings(&app);
    current.server_enabled = enabled;
    settings::write_settings(&app, current);

    server::apply_server_settings(&app);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_server_port(app: AppHandle, port: u16) -> Result<(), String> {
    if port < 1024 {
        return Err("Choose a port above 1023; lower ports are reserved.".into());
    }

    let mut current = settings::get_settings(&app);
    current.server_port = port;
    settings::write_settings(&app, current);

    server::apply_server_settings(&app);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_server_expose_lan(app: AppHandle, expose: bool) -> Result<(), String> {
    let mut current = settings::get_settings(&app);
    current.server_expose_lan = expose;
    settings::write_settings(&app, current);

    server::apply_server_settings(&app);
    Ok(())
}

/// Replace the server token and restart the listener so the old one stops
/// working immediately.
#[tauri::command]
#[specta::specta]
pub fn regenerate_server_token(app: AppHandle) -> Result<String, String> {
    let mut current = settings::get_settings(&app);
    current.server_token = SecretString::default();
    settings::write_settings(&app, current);

    let token = server::ensure_server_token(&app);
    server::apply_server_settings(&app);
    Ok(token)
}

#[tauri::command]
#[specta::specta]
pub fn get_server_status(app: AppHandle) -> Result<ServerStatus, String> {
    use tauri::Manager;

    let settings = settings::get_settings(&app);
    let handle = app.state::<std::sync::Arc<server::ServerHandle>>();
    let bound = handle.bound_addr();

    Ok(ServerStatus {
        running: handle.is_running(),
        bound_address: bound.map(|addr| addr.to_string()),
        client_urls: bound
            .map(|addr| client_urls(addr.port()))
            .unwrap_or_default(),
        token: settings.server_token.expose().to_string(),
    })
}

/// Build the list of URLs a remote client could use to reach this machine.
///
/// Enumerating interfaces properly needs a platform API Handy does not link, so
/// this derives the candidates from the hostname's resolved addresses — which is
/// what a LAN client would look up anyway. Loopback is included last as the
/// same-machine case.
fn client_urls(port: u16) -> Vec<String> {
    let mut urls = Vec::new();

    if let Ok(hostname) = hostname() {
        if let Ok(resolved) = std::net::ToSocketAddrs::to_socket_addrs(&(hostname.as_str(), port)) {
            for addr in resolved {
                // IPv6 literals need brackets in a URL; skip them rather than
                // offering an address most users cannot type back in.
                if let IpAddr::V4(v4) = addr.ip() {
                    if !v4.is_loopback() {
                        urls.push(format!("http://{v4}:{port}"));
                    }
                }
            }
        }
    }

    urls.push(format!("http://127.0.0.1:{port}"));
    urls.dedup();
    urls
}

fn hostname() -> Result<String, std::io::Error> {
    // `gethostname` is not in std; the env var is set on Windows, and
    // `HOST`/`hostname` cover the unix side well enough for a hint in the UI.
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .or_else(|_| std::env::var("HOST"))
        .map_err(|_| std::io::Error::other("hostname is not available"))
}

// ---------------------------------------------------------------------------
// Client-side configuration
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub fn change_client_connection(
    app: AppHandle,
    base_url: String,
    token: String,
) -> Result<(), String> {
    let mut current = settings::get_settings(&app);
    current.client_base_url = base_url.trim().to_string();
    current.client_token = SecretString::new(token.trim());
    settings::write_settings(&app, current);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_client_model(app: AppHandle, model: String) -> Result<(), String> {
    let mut current = settings::get_settings(&app);
    current.client_model = model;
    settings::write_settings(&app, current);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_client_streaming(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut current = settings::get_settings(&app);
    current.client_streaming = enabled;
    settings::write_settings(&app, current);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_client_fallback_local(app: AppHandle, enabled: bool) -> Result<(), String> {
    let mut current = settings::get_settings(&app);
    current.client_fallback_local = enabled;
    settings::write_settings(&app, current);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_client_timeout(app: AppHandle, seconds: u64) -> Result<(), String> {
    if seconds == 0 {
        return Err("Timeout must be at least one second.".into());
    }

    let mut current = settings::get_settings(&app);
    current.client_timeout_secs = seconds;
    settings::write_settings(&app, current);
    Ok(())
}

/// Test the address and token the user typed, without saving them: the panel
/// should be able to validate before committing.
#[tauri::command]
#[specta::specta]
pub async fn test_server_connection(
    base_url: String,
    token: String,
) -> Result<RemoteServerInfo, String> {
    remote_client::probe(&base_url, &token)
        .await
        .map_err(|err| err.to_string())
}

/// List the models available on a remote server, for the client's model picker.
#[tauri::command]
#[specta::specta]
pub async fn list_server_models(
    base_url: String,
    token: String,
) -> Result<Vec<RemoteModel>, String> {
    remote_client::list_models(&base_url, &token)
        .await
        .map_err(|err| err.to_string())
}
