mod artifact_uploader;
mod config;
mod probe;
mod task_runner;
mod ws_client;

use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};
use tokio::sync::watch;

struct AppState {
    ws_stop_tx: Option<watch::Sender<bool>>,
}

#[derive(serde::Serialize)]
struct AgentStatus {
    paired: bool,
    relay_url: String,
    hostname: String,
    agent_machine_id: String,
}

#[derive(serde::Serialize)]
struct RegisterResult {
    paired: bool,
    relay_url: String,
    public_relay_url: String,
    hostname: String,
    agent_machine_id: String,
    link_token: String,
    link_code: String,
}

#[tauri::command]
fn get_status(app: AppHandle, state: State<Mutex<AppState>>) -> AgentStatus {
    let _ = state;
    let cfg = config::load(&app);
    AgentStatus {
        paired: cfg.is_paired(),
        relay_url: cfg.relay_url,
        hostname: cfg.hostname,
        agent_machine_id: cfg.agent_machine_id,
    }
}

const RELAY_URL: &str = env!("VITE_RELAY_URL");
const BOOTSTRAP_TOKEN: &str = env!("VITE_BOOTSTRAP_TOKEN");

#[tauri::command]
fn get_agent_token(app: AppHandle) -> String {
    config::load(&app).token
}

#[derive(serde::Serialize)]
struct BootstrapDefaults {
    relay_url: String,
    has_bootstrap_token: bool,
}

#[tauri::command]
fn get_bootstrap_defaults(app: AppHandle) -> BootstrapDefaults {
    let cfg = config::load(&app);
    // Use saved relay URL override if set, otherwise fall back to build-time constant
    let relay_url = if !cfg.relay_url_override.is_empty() {
        cfg.relay_url_override.clone()
    } else {
        RELAY_URL.trim_end_matches('/').to_string()
    };
    BootstrapDefaults {
        relay_url,
        has_bootstrap_token: !BOOTSTRAP_TOKEN.trim().is_empty(),
    }
}

#[tauri::command]
fn update_relay_url(app: AppHandle, url: String) -> Result<(), String> {
    let mut cfg = config::load(&app);
    cfg.relay_url_override = url.trim_end_matches('/').to_string();
    config::save(&app, &cfg)
}

#[tauri::command]
async fn pair_with_code(
    app: AppHandle,
    state: State<'_, Mutex<AppState>>,
    code: String,
) -> Result<AgentStatus, String> {
    // Relay URL is baked in at build time — user only sees the code field
    let relay_url = RELAY_URL.trim_end_matches('/').to_string();
    let hostname = config::get_hostname();
    let os = "Windows".to_string();

    let claim_url = format!("{}/agent/pair/{}/claim", relay_url, code.trim());
    let client = reqwest::Client::new();
    let resp = client
        .post(&claim_url)
        .json(&serde_json::json!({ "hostname": hostname, "os": os }))
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string))
            .unwrap_or_else(|| format!("Error {status}"));
        return Err(msg);
    }

    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let token = body["token"].as_str().ok_or("No token in response")?.to_string();
    let agent_machine_id = body["agentMachineId"].as_str().ok_or("No agentMachineId")?.to_string();

    let existing = config::load(&app);
    let cfg = config::AgentConfig {
        relay_url: relay_url.clone(),
        token: token.clone(),
        agent_machine_id: agent_machine_id.clone(),
        hostname: hostname.clone(),
        relay_url_override: existing.relay_url_override,
    };
    config::save(&app, &cfg).map_err(|e| format!("Failed to save config: {e}"))?;
    start_ws_connection(&app, &state, relay_url.clone(), token);

    Ok(AgentStatus { paired: true, relay_url, hostname, agent_machine_id })
}

#[tauri::command]
async fn bootstrap_register(
    app: AppHandle,
    state: State<'_, Mutex<AppState>>,
    relay_url: String,
    bootstrap_token: String,
) -> Result<RegisterResult, String> {
    let bootstrap_token = if bootstrap_token.trim().is_empty() || bootstrap_token == "use-baked-token" {
        BOOTSTRAP_TOKEN.to_string()
    } else {
        bootstrap_token
    };
    if bootstrap_token.trim().is_empty() {
        return Err("This Outpost Agent build is missing its bootstrap token".to_string());
    }
    let hostname = config::get_hostname();
    let os = "Windows".to_string();

    let url = format!("{}/agent/bootstrap-register", relay_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "bootstrapToken": bootstrap_token,
            "hostname": hostname,
            "os": os,
        }))
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Registration failed ({status}): {body}"));
    }

    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let agent_token = body["agentToken"].as_str().ok_or("No agentToken in response")?.to_string();
    let agent_machine_id = body["agentMachineId"].as_str().ok_or("No agentMachineId in response")?.to_string();
    let link_token = body["linkToken"].as_str().ok_or("No linkToken in response")?.to_string();
    let link_code = body["linkCode"].as_str().unwrap_or("").to_string();
    // Public URL for the phone QR code — relay returns this from RELAY_PUBLIC_URL env var
    let public_relay_url = body["relayUrl"].as_str().unwrap_or("").to_string();

    let existing = config::load(&app);
    let cfg = config::AgentConfig {
        relay_url: relay_url.clone(),
        token: agent_token.clone(),
        agent_machine_id: agent_machine_id.clone(),
        hostname: hostname.clone(),
        relay_url_override: existing.relay_url_override,
    };
    config::save(&app, &cfg).map_err(|e| format!("Failed to save config: {e}"))?;

    start_ws_connection(&app, &state, relay_url.clone(), agent_token);

    Ok(RegisterResult {
        paired: true,
        relay_url,
        public_relay_url,
        hostname,
        agent_machine_id,
        link_token,
        link_code,
    })
}

#[tauri::command]
async fn pair(
    app: AppHandle,
    state: State<'_, Mutex<AppState>>,
    relay_url: String,
    code: String,
) -> Result<AgentStatus, String> {
    let hostname = config::get_hostname();
    let os = "Windows".to_string();

    let claim_url = format!(
        "{}/agent/pair/{}/claim",
        relay_url.trim_end_matches('/'),
        code.trim()
    );

    let client = reqwest::Client::new();
    let resp = client
        .post(&claim_url)
        .json(&serde_json::json!({ "hostname": hostname, "os": os }))
        .send()
        .await
        .map_err(|e| format!("Network error: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Claim failed ({status}): {body}"));
    }

    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let token = body["token"].as_str().ok_or("No token in response")?.to_string();
    let agent_machine_id = body["agentMachineId"]
        .as_str()
        .ok_or("No agentMachineId in response")?
        .to_string();

    let existing = config::load(&app);
    let cfg = config::AgentConfig {
        relay_url: relay_url.clone(),
        token: token.clone(),
        agent_machine_id: agent_machine_id.clone(),
        hostname: hostname.clone(),
        relay_url_override: existing.relay_url_override,
    };
    config::save(&app, &cfg).map_err(|e| format!("Failed to save config: {e}"))?;

    start_ws_connection(&app, &state, relay_url, token);

    Ok(AgentStatus {
        paired: true,
        relay_url: cfg.relay_url,
        hostname,
        agent_machine_id,
    })
}

#[tauri::command]
fn unpair(app: AppHandle, state: State<Mutex<AppState>>) {
    stop_ws_connection(&state);
    config::clear(&app);
}

#[tauri::command]
async fn check_update(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_updater::UpdaterExt;
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    Ok(update.is_some())
}

fn start_ws_connection(
    app: &AppHandle,
    state: &State<Mutex<AppState>>,
    relay_url: String,
    token: String,
) {
    let (tx, rx) = watch::channel(false);
    let mut guard = state.lock().unwrap();
    if let Some(old_tx) = guard.ws_stop_tx.take() {
        let _ = old_tx.send(true);
    }
    guard.ws_stop_tx = Some(tx);
    ws_client::spawn(app.clone(), relay_url, token, rx);
}

fn stop_ws_connection(state: &State<Mutex<AppState>>) {
    let mut guard = state.lock().unwrap();
    if let Some(tx) = guard.ws_stop_tx.take() {
        let _ = tx.send(true);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(Mutex::new(AppState { ws_stop_tx: None }))
        .setup(|app| {
            // Resolve claude path eagerly so tasks don't depend on a probe running first
            ws_client::init_claude_path();

            let cfg = config::load(app.handle());
            if cfg.is_paired() {
                let state = app.state::<Mutex<AppState>>();
                start_ws_connection(app.handle(), &state, cfg.relay_url, cfg.token);
            }

            // Background update check on startup
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                check_for_update_silently(app_handle).await;
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_agent_token,
            get_bootstrap_defaults,
            update_relay_url,
            bootstrap_register,
            pair_with_code,
            pair,
            unpair,
            check_update
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

async fn check_for_update_silently(app: AppHandle) {
    use tauri_plugin_updater::UpdaterExt;
    let Ok(updater) = app.updater() else { return };
    let Ok(Some(update)) = updater.check().await else { return };
    eprintln!("[updater] new version available: {}", update.version);
    // The dialog: true config handles prompting the user
    if let Err(e) = update.download_and_install(|_, _| {}, || {}).await {
        eprintln!("[updater] install failed: {e}");
    }
}
