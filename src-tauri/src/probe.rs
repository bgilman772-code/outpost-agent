use serde::Serialize;
use std::path::Path;
use std::process::Command;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

trait NoWindow {
    fn no_window(&mut self) -> &mut Self;
}

impl NoWindow for Command {
    fn no_window(&mut self) -> &mut Self {
        #[cfg(target_os = "windows")]
        self.creation_flags(CREATE_NO_WINDOW);
        self
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct ProjectInfo {
    pub name: String,
    pub path: String,
    pub git_remote: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProbeResult {
    pub os: String,
    pub hostname: String,
    pub wsl_available: bool,
    pub wsl_distros: Vec<String>,
    pub claude_installed: bool,
    pub claude_path: String,
    pub ollama_installed: bool,
    pub ollama_running: bool,
    pub ollama_path: String,
    pub ollama_endpoint: String,
    pub ollama_models: Vec<String>,
    pub node_installed: bool,
    pub git_installed: bool,
    pub projects: Vec<ProjectInfo>,
}

pub fn run_probe(hostname: &str) -> ProbeResult {
    let (wsl_available, wsl_distros) = probe_wsl();
    let (claude_installed, claude_path) = probe_claude(&wsl_distros);
    let (ollama_installed, ollama_path) = probe_ollama();
    let ollama_endpoint = "http://localhost:11434".to_string();
    let (ollama_running, ollama_models) = probe_ollama_runtime(&ollama_endpoint);
    let projects = scan_projects();

    ProbeResult {
        os: "Windows".to_string(),
        hostname: hostname.to_string(),
        wsl_available,
        wsl_distros,
        claude_installed,
        claude_path,
        ollama_installed,
        ollama_running,
        ollama_path,
        ollama_endpoint,
        ollama_models,
        node_installed: command_exists_win("node", "--version"),
        git_installed: command_exists_win("git", "--version"),
        projects,
    }
}

fn command_exists_win(cmd: &str, arg: &str) -> bool {
    Command::new("cmd")
        .args(["/C", cmd, arg])
        .no_window()
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn probe_wsl() -> (bool, Vec<String>) {
    let output = Command::new("wsl")
        .args(["--list", "--quiet"])
        .no_window()
        .output();

    match output {
        Ok(out) if out.status.success() => {
            // WSL output uses UTF-16LE on older Windows; try UTF-8 first
            let raw = String::from_utf8_lossy(&out.stdout);
            let distros: Vec<String> = raw
                .lines()
                .map(|l| l.trim().trim_matches('\0').to_string())
                .filter(|l| !l.is_empty())
                .collect();
            (true, distros)
        }
        _ => (false, vec![]),
    }
}

/// Returns true only if the resolved claude path is in a known-safe location.
fn is_expected_claude_location(path: &str) -> bool {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return false;
    }
    for var in &["APPDATA", "LOCALAPPDATA", "PROGRAMFILES", "USERPROFILE"] {
        if let Ok(dir) = std::env::var(var) {
            if !dir.is_empty() && p.starts_with(&dir) {
                return true;
            }
        }
    }
    false
}

pub fn resolve_claude_exe(cmd_path: &str) -> String {
    let cmd_dir = match std::path::Path::new(cmd_path).parent() {
        Some(d) => d.to_path_buf(),
        None => return cmd_path.to_string(),
    };

    // claude.cmd in npm bin dir ships alongside the real exe at:
    // <bin_dir>/node_modules/@anthropic-ai/claude-code/bin/claude.exe
    let candidate = cmd_dir
        .join("node_modules")
        .join("@anthropic-ai")
        .join("claude-code")
        .join("bin")
        .join("claude.exe");
    if candidate.exists() && is_expected_claude_location(&candidate.to_string_lossy()) {
        return candidate.to_string_lossy().to_string();
    }

    // Fallback: scan the .cmd for any line mentioning claude.exe and resolve it
    if let Ok(contents) = std::fs::read_to_string(cmd_path) {
        for line in contents.lines() {
            let line = line.trim();
            if line.contains("claude.exe") && !line.starts_with("REM") && !line.starts_with("::") {
                // Strip surrounding quotes and variable references
                let cleaned = line
                    .replace("%dp0%", &cmd_dir.to_string_lossy())
                    .replace("%~dp0", &cmd_dir.to_string_lossy())
                    .replace('"', "")
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string();
                let p = std::path::Path::new(&cleaned);
                // Validate path is in a known-safe location before trusting it
                if p.exists() && is_expected_claude_location(&p.to_string_lossy()) {
                    return p.to_string_lossy().to_string();
                }
            }
        }
    }

    cmd_path.to_string()
}

fn probe_claude(wsl_distros: &[String]) -> (bool, String) {
    // Try `where claude` to get the full path on Windows
    if let Ok(out) = Command::new("cmd")
        .args(["/C", "where", "claude"])
        .no_window()
        .output()
    {
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !path.is_empty() {
                // If it's a .cmd wrapper, resolve to the actual .exe
                let resolved = if path.to_lowercase().ends_with(".cmd") {
                    resolve_claude_exe(&path)
                } else {
                    path
                };
                return (true, resolved);
            }
        }
    }

    // Check inside WSL distros
    for distro in wsl_distros {
        let out = Command::new("wsl")
            .args(["-d", distro, "--", "bash", "-c", "which claude"])
            .no_window()
            .output();
        if let Ok(o) = out {
            if o.status.success() {
                let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !path.is_empty() {
                    return (true, format!("wsl:{distro}:{path}"));
                }
            }
        }
    }

    (false, String::new())
}

fn probe_ollama() -> (bool, String) {
    if let Ok(out) = Command::new("cmd")
        .args(["/C", "where", "ollama"])
        .no_window()
        .output()
    {
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !path.is_empty() {
                return (true, path);
            }
        }
    }
    // Fallback: winget installs Ollama here but doesn't update the running process's PATH,
    // so `where ollama` fails immediately after a fresh install even though the exe exists.
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let candidate = std::path::PathBuf::from(local_app_data)
            .join("Programs")
            .join("Ollama")
            .join("ollama.exe");
        if candidate.exists() {
            return (true, candidate.to_string_lossy().to_string());
        }
    }
    (false, String::new())
}

fn probe_ollama_runtime(endpoint: &str) -> (bool, Vec<String>) {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build();
    let Ok(client) = client else { return (false, vec![]); };
    let Ok(resp) = client.get(format!("{}/api/tags", endpoint.trim_end_matches('/'))).send() else {
        return (false, vec![]);
    };
    if !resp.status().is_success() {
        return (false, vec![]);
    }
    let Ok(body) = resp.json::<serde_json::Value>() else {
        return (true, vec![]);
    };
    let models = body["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| model["name"].as_str().map(|name| name.to_string()))
        .collect::<Vec<_>>();
    (true, models)
}

fn git_remote_for(path: &Path) -> Option<String> {
    Command::new("git")
        .args(["-C", path.to_str().unwrap_or("."), "remote", "get-url", "origin"])
        .no_window()
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

fn scan_projects() -> Vec<ProjectInfo> {
    let mut projects = Vec::new();

    // Search roots: user home + common project dirs
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".to_string());
    let search_roots = vec![
        format!("{home}\\source"),
        format!("{home}\\repos"),
        format!("{home}\\projects"),
        format!("{home}\\code"),
        format!("{home}\\dev"),
        format!("{home}\\Documents"),
        format!("{home}\\Desktop"),
        home.clone(),
    ];

    for root in &search_roots {
        let root_path = Path::new(root);
        if !root_path.exists() {
            continue;
        }

        // Root itself might be a git repo
        if is_git_repo(root_path) {
            let name = root_path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| root.clone());
            projects.push(ProjectInfo {
                name,
                path: root.clone(),
                git_remote: git_remote_for(root_path),
            });
            continue;
        }

        // One level deep
        if let Ok(entries) = std::fs::read_dir(root_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && is_git_repo(&path) {
                    let name = path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.to_string_lossy().to_string());
                    let path_str = path.to_string_lossy().to_string();
                    if !projects.iter().any(|p: &ProjectInfo| p.path == path_str) {
                        projects.push(ProjectInfo {
                            name,
                            path: path_str,
                            git_remote: git_remote_for(&path),
                        });
                    }
                }
            }
        }
    }

    projects.sort_by(|a, b| a.name.cmp(&b.name));
    projects.dedup_by(|a, b| a.path == b.path);
    projects
}
