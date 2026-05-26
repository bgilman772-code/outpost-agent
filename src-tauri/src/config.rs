use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::Manager;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    pub relay_url: String,

    // Intentionally NOT serialized. Tokens live in the OS credential store (keyring).
    // This field is deserialized only so that legacy config.json values can be read
    // once for migration and then discarded. It is never written back to disk.
    #[serde(default, skip_serializing)]
    pub token: String,

    pub agent_machine_id: String,
    pub hostname: String,

    #[serde(default)]
    pub relay_url_override: String,

    /// Unix timestamp (seconds) when the current token was issued.
    /// Stored here (without the token) to support expiration checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_issued_at: Option<i64>,
}

impl AgentConfig {
    /// Returns true when relay_url and agent_machine_id are both present.
    /// Does NOT check for a token — use credentials::load_token() for that.
    pub fn is_paired(&self) -> bool {
        !self.relay_url.is_empty() && !self.agent_machine_id.is_empty()
    }
}

fn config_path(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .expect("app data dir")
        .join("config.json")
}

pub fn load(app: &tauri::AppHandle) -> AgentConfig {
    let path = config_path(app);
    if let Ok(contents) = fs::read_to_string(&path) {
        serde_json::from_str(&contents).unwrap_or_default()
    } else {
        AgentConfig::default()
    }
}

pub fn save(app: &tauri::AppHandle, cfg: &AgentConfig) -> Result<(), String> {
    let path = config_path(app);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // serde will not emit `token` (skip_serializing), so the file is always clean.
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    fs::write(&path, json).map_err(|e| e.to_string())
}

pub fn clear(app: &tauri::AppHandle) {
    let path = config_path(app);
    let _ = fs::remove_file(path);
}

pub fn get_hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}
