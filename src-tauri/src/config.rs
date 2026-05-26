use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::Manager;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    pub relay_url: String,
    pub token: String,
    pub agent_machine_id: String,
    pub hostname: String,
    #[serde(default)]
    pub relay_url_override: String,
}

impl AgentConfig {
    pub fn is_paired(&self) -> bool {
        !self.token.is_empty()
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
