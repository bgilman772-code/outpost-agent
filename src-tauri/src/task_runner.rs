use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use tokio::sync::mpsc;
use serde_json::json;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub enum TaskEvent {
    Output { data: String, stream: &'static str },
    Done { exit_code: i32 },
    Error(String),
}

// ── Task cancellation registry ────────────────────────────────────────────────
// Maps an in-flight task_id to a cancel flag (+ child PID for the CLI path) so a
// `cancel_task` message from the phone can stop a running task.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

struct TaskHandle {
    cancel: Arc<AtomicBool>,
    pid: Option<u32>,
}

fn task_registry() -> &'static Mutex<std::collections::HashMap<String, TaskHandle>> {
    static REG: OnceLock<Mutex<std::collections::HashMap<String, TaskHandle>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn register_task(task_id: &str) -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    task_registry()
        .lock()
        .unwrap()
        .insert(task_id.to_string(), TaskHandle { cancel: cancel.clone(), pid: None });
    cancel
}

fn set_task_pid(task_id: &str, pid: u32) {
    if let Some(h) = task_registry().lock().unwrap().get_mut(task_id) {
        h.pid = Some(pid);
    }
}

fn unregister_task(task_id: &str) {
    task_registry().lock().unwrap().remove(task_id);
}

/// Request cancellation of a running task. Sets its cancel flag and, for the CLI
/// (Claude/Codex) path, kills the child process tree. Returns true if the task
/// was known/running.
pub fn cancel_task(task_id: &str) -> bool {
    let entry = task_registry()
        .lock()
        .unwrap()
        .get(task_id)
        .map(|h| (h.cancel.clone(), h.pid));
    match entry {
        Some((cancel, pid)) => {
            cancel.store(true, Ordering::SeqCst);
            if let Some(pid) = pid {
                kill_process_tree(pid);
            }
            true
        }
        None => false,
    }
}

fn kill_process_tree(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string(), "/T", "/F"]);
        cmd.creation_flags(CREATE_NO_WINDOW);
        let _ = cmd.spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Graceful TERM, then a KILL shortly after in case it ignores TERM.
        let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).spawn();
        let pid_str = pid.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let _ = Command::new("kill").args(["-KILL", &pid_str]).spawn();
        });
    }
}

/// RAII guard that unregisters a task when the worker thread exits by any path.
struct TaskGuard(String);
impl Drop for TaskGuard {
    fn drop(&mut self) {
        unregister_task(&self.0);
    }
}

#[derive(Clone)]
pub struct TaskEngine {
    pub provider_id: String,
    pub model_id: Option<String>,
    pub endpoint: Option<String>,
    pub api_key: Option<String>,
    pub is_code_task: bool,
}

/// Returns true if `path` is within the current user's home directory.
pub fn is_path_within_home(path: &str) -> bool {
    let p = std::path::Path::new(path);
    for var in &["USERPROFILE", "HOME"] {
        if let Ok(home) = std::env::var(var) {
            if !home.is_empty() && p.starts_with(&home) {
                return true;
            }
        }
    }
    false
}

/// Returns Ok if the endpoint is a loopback address (localhost / 127.0.0.1).
pub fn validate_ollama_endpoint(endpoint: &str) -> Result<(), String> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    let allowed = trimmed == "http://localhost:11434"
        || trimmed == "http://localhost"
        || trimmed == "http://127.0.0.1"
        || trimmed.starts_with("http://localhost:")
        || trimmed.starts_with("http://127.0.0.1:");
    if allowed {
        Ok(())
    } else {
        Err(format!(
            "Ollama endpoint must be localhost or 127.0.0.1, got: {endpoint}"
        ))
    }
}

/// Returns Ok if `url` uses a safe git transport (https, ssh, git@).
fn validate_repo_url(url: &str) -> Result<(), String> {
    if url.starts_with("https://") || url.starts_with("git@") || url.starts_with("ssh://") {
        Ok(())
    } else {
        Err("Repository URL must use https://, ssh://, or git@ protocol".to_string())
    }
}

/// Ensure the project has CLAUDE.md and an outpost-output/ folder before running a task.
pub fn ensure_project_setup(project_path: &str) {
    let path = std::path::Path::new(project_path);

    // Always create the output folder so Claude can use it
    let output_dir = path.join("outpost-output");
    let _ = std::fs::create_dir_all(&output_dir);

    // Create CLAUDE.md if it doesn't already exist
    let claude_md = path.join("CLAUDE.md");
    if !claude_md.exists() {
        let project_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| project_path.to_string());
        let content = format!(
            "# {project_name}\n\n\
             ## Outpost Output\n\n\
             When creating files for the user to review or download (documents, reports, \
             generated code, exports, data files, etc.), save them to the `outpost-output/` \
             directory in the project root. The Outpost mobile app monitors this folder and \
             automatically makes those files available for download on the user's phone.\n\n\
             Use short, descriptive filenames that summarize the content — like \
             `Quarterly_Sales_Report.pdf`, `Marketing_Plan.md`, or `Data_Cleanup_Script.py`. \
             Keep the base name under 40 characters. No timestamps, UUIDs, or verbose \
             descriptions in the filename.\n"
        );
        let _ = std::fs::write(&claude_md, content);
    }
}

/// Read project-specific agent instructions from AGENTS.md in the project root.
/// This mirrors Codex's AGENTS.md convention — a file where developers describe
/// how to work on the project (test commands, coding standards, key files, etc.).
fn load_project_instructions(project_path: &str) -> Option<String> {
    for filename in &["AGENTS.md", "OUTPOST.md"] {
        let path = std::path::Path::new(project_path).join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed = content.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

fn session_memory_path(project_path: &str) -> std::path::PathBuf {
    std::path::Path::new(project_path).join(".outpost-session-memory.md")
}

fn write_session_memory(project_path: &str, session_memory: &str) -> Result<std::path::PathBuf, String> {
    let path = session_memory_path(project_path);
    let redacted = redact_secrets(session_memory);
    std::fs::write(&path, redacted)
        .map_err(|e| format!("Could not write session memory: {e}"))?;
    Ok(path)
}

fn compose_code_task_prompt(prompt: &str, session_memory: Option<&str>, project_path: &str) -> String {
    let code_session_rules = "You are in an Outpost code session.\n\
- Treat this as in-place project work: edit existing project files or add new files directly in the codebase.\n\
- If the user asks you to output, export, download, send to phone, create a report, generate a document, produce a mockup, or deliver any standalone file — you MUST save the finished file(s) into the `outpost-output/` directory in the project root. This directory already exists. The Outpost mobile app only surfaces files saved there, so a deliverable saved anywhere else will not reach the user.\n\
- This applies to every deliverable, including multi-file outputs (e.g. a website's .html/.css/.js, or a report plus its images): write the complete, final set of files into `outpost-output/`. Do not leave them in the project root, a temp folder, or a new subfolder elsewhere.\n\
- For all other requests (edits, inspections, explanations, analysis, answers), work in-place and respond through the session. Do not create files in `outpost-output/` for those.\n\n";

    // Inject AGENTS.md / OUTPOST.md project instructions if present.
    let agents_prefix = load_project_instructions(project_path)
        .map(|instructions| format!("Project instructions (AGENTS.md):\n{}\n\n", instructions))
        .unwrap_or_default();

    if let Some(memory) = session_memory {
        if !memory.trim().is_empty() {
            return format!(
                "{code_session_rules}{agents_prefix}You are continuing an existing Outpost coding session.\n\
The file `.outpost-session-memory.md` in the project root has been refreshed with recent session context.\n\
Use that memory as authoritative context for follow-up references and continue the same thread unless the user explicitly changes direction.\n\
Do not ask the user to restate prior context unless there is true ambiguity.\n\n\
Session memory:\n{}\n\n\
New user request:\n{}",
                memory.trim(),
                prompt
            );
        }
    }
    format!("{code_session_rules}{agents_prefix}{prompt}")
}

fn redact_secrets(input: &str) -> String {
    let mut out = input.to_string();
    const DIRECT_PATTERNS: &[&str] = &[
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----BEGIN OPENSSH PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----",
        "-----BEGIN DSA PRIVATE KEY-----",
        "-----BEGIN PRIVATE KEY-----",
    ];
    for marker in DIRECT_PATTERNS {
        if out.contains(marker) {
            out = out.replace(marker, "[REDACTED_PRIVATE_KEY]");
        }
    }

    // Redact common provider token prefixes by tokenizing on whitespace and quotes.
    let mut rebuilt = String::with_capacity(out.len());
    for part in out.split_inclusive(char::is_whitespace) {
        let trimmed = part.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c.is_whitespace());
        // Also consider the value side of `KEY=value` / `KEY:value` assignments,
        // since `printenv` / `.env` output (e.g. `GITHUB_TOKEN=ghp_…`) puts the
        // secret after the prefix rather than at the start of the token.
        let value_candidate = trimmed.rsplit(|c| c == '=' || c == ':').next().unwrap_or(trimmed);
        let matches_secret = |s: &str| {
            let lower = s.to_lowercase();
            lower.starts_with("sk-ant-")
                || lower.starts_with("sk-proj-")
                || lower.starts_with("ghp_")
                || lower.starts_with("gho_")
                || lower.starts_with("ghs_")
                || lower.starts_with("ghr_")
                || lower.starts_with("github_pat_")
                || lower.starts_with("xoxb-")
                || lower.starts_with("xoxp-")
                || lower.starts_with("xoxa-")
                || lower.starts_with("xoxr-")
                || looks_like_aws_access_key(s)
                || looks_like_jwt(s)
        };
        let should_redact = matches_secret(trimmed) || matches_secret(value_candidate);
        if should_redact {
            let suffix = if part.ends_with(char::is_whitespace) {
                part.chars().rev().take_while(|c| c.is_whitespace()).collect::<String>().chars().rev().collect::<String>()
            } else {
                String::new()
            };
            rebuilt.push_str("[REDACTED_SECRET]");
            rebuilt.push_str(&suffix);
        } else {
            rebuilt.push_str(part);
        }
    }
    rebuilt
}

fn looks_like_aws_access_key(token: &str) -> bool {
    let t = token.trim();
    if t.len() != 20 || !t.starts_with("AKIA") {
        return false;
    }
    t[4..].chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

fn looks_like_jwt(token: &str) -> bool {
    let t = token.trim().trim_matches('.');
    let parts: Vec<&str> = t.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    parts.iter().all(|p| p.len() >= 8 && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

/// Create a new project folder and set it up with CLAUDE.md + outpost-output/.
/// Returns the absolute path to the created folder.
pub fn create_project(name: &str, parent_dir: Option<&str>) -> Result<String, String> {
    let base = if let Some(dir) = parent_dir {
        if !dir.is_empty() {
            std::path::PathBuf::from(dir)
        } else {
            default_projects_dir()
        }
    } else {
        default_projects_dir()
    };

    // Sanitize: strip path separators from the name so users can't escape the parent dir
    let safe_name: String = name.chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    let safe_name = safe_name.trim().to_string();
    if safe_name.is_empty() {
        return Err("Project name is empty after sanitization".to_string());
    }

    let project_path = base.join(&safe_name);
    std::fs::create_dir_all(&project_path)
        .map_err(|e| format!("Could not create directory: {e}"))?;

    let path_str = project_path.to_string_lossy().to_string();
    ensure_project_setup(&path_str);
    Ok(path_str)
}

fn default_projects_dir() -> std::path::PathBuf {
    // %USERPROFILE%\Projects on Windows, ~ on other platforms
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return std::path::PathBuf::from(profile).join("Projects");
    }
    if let Ok(home) = std::env::var("HOME") {
        return std::path::PathBuf::from(home).join("Projects");
    }
    std::path::PathBuf::from(".")
}

/// Clone a GitHub repo into the default projects directory and set it up.
/// `repo_url` should be the authenticated HTTPS URL (with token embedded).
/// Returns the absolute path to the cloned directory.
pub fn clone_repo(repo_url: &str, name: &str) -> Result<String, String> {
    validate_repo_url(repo_url)?;
    let base = default_projects_dir();
    std::fs::create_dir_all(&base)
        .map_err(|e| format!("Could not create projects directory: {e}"))?;

    let safe_name: String = name.chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    let safe_name = safe_name.trim().to_string();
    if safe_name.is_empty() {
        return Err("Repository name is empty after sanitization".to_string());
    }

    let dest = base.join(&safe_name);
    if dest.exists() {
        return Err(format!("Directory already exists: {}", dest.display()));
    }

    #[allow(unused_mut)]
    let mut cmd = Command::new("git");
    cmd.args(["clone", repo_url, &dest.to_string_lossy()]);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let output = cmd.output()
        .map_err(|e| format!("git clone failed to start: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Redact any embedded token from error message
        let msg = stderr.replace(repo_url, "<repo_url>");
        return Err(format!("git clone failed: {}", msg.trim()));
    }

    let path_str = dest.to_string_lossy().to_string();
    ensure_project_setup(&path_str);
    Ok(path_str)
}

fn run_git_in(args: &[&str], cwd: &str) -> Result<String, String> {
    validate_git_command(args)?;
    #[allow(unused_mut)]
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(cwd);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let output = cmd.output()
        .map_err(|e| format!("git command failed to start: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        return Err(format!("{stdout}{stderr}").trim().to_string());
    }
    Ok(format!("{stdout}{stderr}"))
}

fn validate_git_command(args: &[&str]) -> Result<(), String> {
    if args.is_empty() {
        return Err("Git command denied: empty args".to_string());
    }
    let allowed = match args[0] {
        "add" => args.len() == 2 && args[1] == "-A",
        "diff" => args.len() == 3 && args[1] == "--cached" && args[2] == "--quiet",
        "commit" => args.len() == 3 && args[1] == "-m" && !args[2].trim().is_empty(),
        "remote" => args.len() == 3 && args[1] == "get-url" && args[2] == "origin",
        "push" => {
            if args.len() == 1 {
                true
            } else if args.len() == 2 {
                !args[1].starts_with('-')
            } else {
                false
            }
        }
        _ => false,
    };
    if !allowed {
        return Err(format!("Git command denied by policy: {:?}", args));
    }
    Ok(())
}

/// Commit all changed files and push to the configured git remote.
/// If `github_token` is supplied and the remote is an HTTPS GitHub URL, the
/// token is embedded for this push only (the stored remote URL is not modified).
pub fn git_push(project_path: &str, commit_message: &str, github_token: Option<&str>) -> Result<String, String> {
    if !is_path_within_home(project_path) {
        return Err("Project path must be within the user's home directory".to_string());
    }
    let _ = run_git_in(&["add", "-A"], project_path)?;

    // Check if there's anything staged
    #[allow(unused_mut)]
    let mut status_cmd = Command::new("git");
    status_cmd.args(["diff", "--cached", "--quiet"]).current_dir(project_path);
    #[cfg(target_os = "windows")]
    status_cmd.creation_flags(CREATE_NO_WINDOW);
    let status = status_cmd.output()
        .map_err(|e| format!("git status failed: {e}"))?;

    if status.status.success() {
        // exit 0 = nothing staged
        return Ok("Nothing to commit, working tree clean.".to_string());
    }

    let commit_out = run_git_in(&["commit", "-m", commit_message], project_path)?;

    let push_out = if let Some(token) = github_token {
        let remote_url = run_git_in(&["remote", "get-url", "origin"], project_path)
            .unwrap_or_default();
        let remote_url = remote_url.trim();
        if remote_url.starts_with("https://") && remote_url.contains("github.com") {
            run_git_push_authenticated(token, remote_url, project_path)?
        } else {
            run_git_in(&["push"], project_path)?
        }
    } else {
        run_git_in(&["push"], project_path)?
    };

    Ok(format!("{commit_out}\n{push_out}").trim().to_string())
}

/// Push to a GitHub HTTPS remote using GIT_ASKPASS so the token is never part
/// of the git command's argument list (visible via /proc/PID/cmdline or ps).
fn run_git_push_authenticated(token: &str, remote_url: &str, project_path: &str) -> Result<String, String> {
    let clean = regex_strip_creds(remote_url);

    let tmp_dir = std::env::temp_dir();
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    #[cfg(unix)]
    let (script_path, script_content) = {
        let p = tmp_dir.join(format!("git-askpass-{pid}-{ts}.sh"));
        let escaped = token.replace('\'', "'\\''");
        let c = format!("#!/bin/sh\necho '{}'\n", escaped);
        (p, c)
    };
    #[cfg(not(unix))]
    let (script_path, script_content) = {
        let p = tmp_dir.join(format!("git-askpass-{pid}-{ts}.bat"));
        let escaped = token.replace('%', "%%");
        let c = format!("@echo off\r\necho {}\r\n", escaped);
        (p, c)
    };

    std::fs::write(&script_path, script_content.as_bytes())
        .map_err(|e| format!("failed to write askpass helper: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("failed to chmod askpass helper: {e}"))?;
    }

    let script_str = script_path.to_string_lossy().to_string();

    #[allow(unused_mut)]
    let mut cmd = Command::new("git");
    cmd.args(["push", &clean])
        .current_dir(project_path)
        .env("GIT_ASKPASS", &script_str)
        .env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let output = cmd.output()
        .map_err(|e| format!("git push failed to start: {e}"))?;

    let _ = std::fs::remove_file(&script_path);

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        return Err(format!("{stdout}{stderr}").trim().to_string());
    }
    Ok(format!("{stdout}{stderr}"))
}

/// Strip embedded credentials from an HTTPS URL: https://user:pass@host → https://host
fn regex_strip_creds(url: &str) -> String {
    if let Some(at_pos) = url.find('@') {
        if let Some(scheme_end) = url.find("://") {
            let scheme = &url[..scheme_end + 3]; // "https://"
            let rest = &url[at_pos + 1..];       // "github.com/..."
            return format!("{scheme}{rest}");
        }
    }
    url.to_string()
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectFileEntry {
    pub relative_path: String,
    pub name: String,
    pub extension: String,
    pub size: u64,
    pub modified_at: String,
    pub is_likely_data: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
    pub importable: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryBrowseResult {
    pub current_path: Option<String>,
    pub parent_path: Option<String>,
    pub entries: Vec<DirectoryEntry>,
}

pub fn list_project_files(project_path: &str) -> Result<Vec<ProjectFileEntry>, String> {
    let base = std::path::PathBuf::from(project_path);
    if !base.exists() || !base.is_dir() {
        return Err("Project path does not exist".to_string());
    }

    let mut results = Vec::new();
    collect_project_files(&base, &base, 0, &mut results)?;
    results.sort_by(|a, b| {
        b.is_likely_data
            .cmp(&a.is_likely_data)
            .then_with(|| a.relative_path.cmp(&b.relative_path))
    });
    results.truncate(60);

    Ok(results)
}

pub fn list_directories(path: Option<&str>) -> Result<DirectoryBrowseResult, String> {
    if let Some(path) = path {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return list_root_directories();
        }
        return list_child_directories(trimmed);
    }
    list_root_directories()
}

fn list_root_directories() -> Result<DirectoryBrowseResult, String> {
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for drive in discover_windows_drives() {
        if seen.insert(drive.to_lowercase()) {
            entries.push(DirectoryEntry {
                name: drive.clone(),
                path: drive,
                importable: true,
            });
        }
    }

    if let Ok(home) = std::env::var("USERPROFILE") {
        let favorites = [
            ("Home", std::path::PathBuf::from(&home)),
            ("Desktop", std::path::PathBuf::from(&home).join("Desktop")),
            ("Documents", std::path::PathBuf::from(&home).join("Documents")),
            ("Projects", std::path::PathBuf::from(&home).join("Projects")),
            ("Downloads", std::path::PathBuf::from(&home).join("Downloads")),
        ];
        for (label, path) in favorites {
            if path.exists() {
                let path_str = path.to_string_lossy().to_string();
                if seen.insert(path_str.to_lowercase()) {
                    entries.push(DirectoryEntry {
                        name: label.to_string(),
                        path: path_str,
                        importable: true,
                    });
                }
            }
        }
    }

    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(DirectoryBrowseResult {
        current_path: None,
        parent_path: None,
        entries,
    })
}

fn list_child_directories(path: &str) -> Result<DirectoryBrowseResult, String> {
    let base = std::path::PathBuf::from(path);
    if !base.exists() || !base.is_dir() {
        return Err("Directory does not exist".to_string());
    }
    // Restrict directory browsing to the user's home directory to prevent full-fs enumeration
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    if !home.is_empty() && !base.starts_with(&home) {
        return Err("Directory browsing is restricted to the user's home directory".to_string());
    }

    let mut entries = Vec::new();
    let read_dir = std::fs::read_dir(&base).map_err(|e| format!("Could not read directory: {e}"))?;
    for entry in read_dir {
        let entry = entry.map_err(|e| format!("Could not inspect directory: {e}"))?;
        let child = entry.path();
        if !child.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if should_skip_browse_dir(&name) {
            continue;
        }
        entries.push(DirectoryEntry {
            name,
            path: child.to_string_lossy().to_string(),
            importable: true,
        });
    }
    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    let parent_path = base.parent().map(|parent| parent.to_string_lossy().to_string());
    Ok(DirectoryBrowseResult {
        current_path: Some(base.to_string_lossy().to_string()),
        parent_path,
        entries,
    })
}

fn discover_windows_drives() -> Vec<String> {
    let mut drives = Vec::new();
    for letter in 'C'..='Z' {
        let candidate = format!("{}:\\", letter);
        if std::path::Path::new(&candidate).exists() {
            drives.push(candidate);
        }
    }
    drives
}

fn should_skip_browse_dir(name: &str) -> bool {
    matches!(
        name,
        "$Recycle.Bin" | "System Volume Information" | "node_modules" | ".git" | "dist" | "build" | "outpost-output"
    )
}

fn collect_project_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    results: &mut Vec<ProjectFileEntry>,
) -> Result<(), String> {
    use std::time::UNIX_EPOCH;

    if depth > 2 {
        return Ok(());
    }

    let entries = std::fs::read_dir(dir).map_err(|e| format!("Could not read project directory: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Could not inspect project directory: {e}"))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if should_skip_path(&name) {
          continue;
        }

        if path.is_dir() {
            collect_project_files(root, &path, depth + 1, results)?;
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.len() > 50 * 1024 * 1024 {
            continue;
        }

        let relative_path = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_lowercase();
        let modified_at = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs().to_string())
            .unwrap_or_default();

        results.push(ProjectFileEntry {
            relative_path,
            name,
            extension: extension.clone(),
            size: metadata.len(),
            modified_at,
            is_likely_data: is_likely_data_file(&extension),
        });
    }

    Ok(())
}

fn should_skip_path(name: &str) -> bool {
    matches!(
        name,
        ".git" | "node_modules" | ".next" | "dist" | "build" | "coverage" | "target" | "outpost-output"
    )
}

fn is_likely_data_file(extension: &str) -> bool {
    matches!(
        extension,
        "csv" | "tsv" | "xlsx" | "xls" | "json" | "parquet" | "ndjson" | "txt"
    )
}

pub fn setup_ollama(model_id: &str, endpoint: &str) -> Result<crate::probe::ProbeResult, String> {
    let initial_probe = crate::probe::run_probe("local");
    let installed = initial_probe.ollama_installed;
    let path = initial_probe.ollama_path.clone();
    let ollama_path = if installed {
        path
    } else {
        install_ollama()?;
        let probe = crate::probe::run_probe("local");
        if !probe.ollama_installed {
            return Err("Ollama install finished but the executable was not found".to_string());
        }
        probe.ollama_path
    };

    if !is_ollama_running(endpoint) {
        start_ollama_service(&ollama_path)?;
        wait_for_ollama(endpoint, 45)?;
    }

    if !model_already_pulled(&ollama_path, model_id) {
        pull_ollama_model(&ollama_path, model_id)?;
    }
    Ok(crate::probe::run_probe("local"))
}

fn install_ollama() -> Result<(), String> {
    #[allow(unused_mut)]
    let mut cmd = Command::new("winget");
    cmd.args(["install", "-e", "--id", "Ollama.Ollama", "--accept-package-agreements", "--accept-source-agreements"]);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = cmd.output().map_err(|e| format!("Could not start Ollama installer: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "Ollama install failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

fn is_ollama_running(endpoint: &str) -> bool {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build();
    let Ok(client) = client else { return false; };
    client
        .get(format!("{}/api/tags", endpoint.trim_end_matches('/')))
        .send()
        .map(|resp| resp.status().is_success())
        .unwrap_or(false)
}

fn start_ollama_service(ollama_path: &str) -> Result<(), String> {
    #[allow(unused_mut)]
    let mut cmd = Command::new(ollama_path);
    cmd.arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.spawn().map_err(|e| format!("Could not start Ollama service: {e}"))?;
    Ok(())
}

fn wait_for_ollama(endpoint: &str, seconds: u64) -> Result<(), String> {
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(seconds) {
        if is_ollama_running(endpoint) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    Err("Ollama did not become ready in time".to_string())
}

fn model_already_pulled(ollama_path: &str, model_id: &str) -> bool {
    #[allow(unused_mut)]
    let mut cmd = Command::new(ollama_path);
    cmd.arg("list");
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains(model_id))
        .unwrap_or(false)
}

fn pull_ollama_model(ollama_path: &str, model_id: &str) -> Result<(), String> {
    #[allow(unused_mut)]
    let mut cmd = Command::new(ollama_path);
    cmd.args(["pull", model_id]);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = cmd.output().map_err(|e| format!("Could not pull Ollama model: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "Could not pull Ollama model {}: {}",
            model_id,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Run `claude --print --output-format stream-json "<prompt>"` in the given project directory.
/// If use_wsl is true, run inside the specified WSL distro.
/// Events are sent on the returned channel.
pub fn spawn_task(
    task_id: String,
    project_path: String,
    prompt: String,
    session_memory: Option<String>,
    engine: TaskEngine,
    use_wsl: bool,
    wsl_distro: Option<String>,
    claude_path: String,
    extra_env: std::collections::HashMap<String, String>,
) -> mpsc::Receiver<TaskEvent> {
    // Bounded so a very chatty task (e.g. a build dumping megabytes to stdout)
    // can't grow memory without limit when the WSS consumer is slower than the
    // producer. blocking_send on the reader threads applies backpressure all the
    // way down to the child process's stdout pipe, which is the desired behavior.
    let (tx, rx) = mpsc::channel(512);

    ensure_project_setup(&project_path);
    if let Some(memory) = session_memory.as_deref() {
        let _ = write_session_memory(&project_path, memory);
    }

    // Register this task so a `cancel_task` message can stop it. The guard inside
    // the worker thread unregisters it on every exit path.
    let cancel = register_task(&task_id);

    std::thread::spawn(move || {
        let _guard = TaskGuard(task_id.clone());
        let prompt = redact_secrets(&prompt);
        let session_memory = session_memory.map(|value| redact_secrets(&value));
        if engine.provider_id == "ollama" {
            if engine.is_code_task {
                run_ollama_code_task(&tx, &project_path, &prompt, session_memory.as_deref(), &engine, &cancel);
            } else {
                run_ollama_task(&tx, &project_path, &prompt, &engine);
            }
            let _ = tx.blocking_send(TaskEvent::Done { exit_code: 0 });
            return;
        }

        if engine.provider_id == "gemini" {
            match engine.api_key.as_deref() {
                Some(key) if !key.is_empty() => {
                    if engine.is_code_task {
                        run_gemini_code_task(&tx, &project_path, &prompt, session_memory.as_deref(), &engine, key, &cancel);
                    } else {
                        run_gemini_task(&tx, &project_path, &prompt, &engine, key);
                    }
                }
                _ => {
                    let _ = tx.blocking_send(TaskEvent::Error("Gemini API key not configured. Add it in Profile > AI Engine.".to_string()));
                }
            }
            let _ = tx.blocking_send(TaskEvent::Done { exit_code: 0 });
            return;
        }

        if matches!(engine.provider_id.as_str(), "openai" | "groq" | "mistral") {
            let (base_url, default_model, display_name) = match engine.provider_id.as_str() {
                "openai"  => ("https://api.openai.com/v1",           "gpt-4o",                    "OpenAI"),
                "groq"    => ("https://api.groq.com/openai/v1",      "llama-3.3-70b-versatile",   "Groq"),
                "mistral" => ("https://api.mistral.ai/v1",           "mistral-large-latest",      "Mistral"),
                _         => unreachable!(),
            };
            match engine.api_key.as_deref() {
                Some(key) if !key.is_empty() => {
                    if engine.is_code_task {
                        run_openai_compat_code_task(&tx, &project_path, &prompt, session_memory.as_deref(), &engine, key, base_url, default_model, &cancel);
                    } else {
                        run_openai_compat_task(&tx, &project_path, &prompt, &engine, key, base_url, default_model);
                    }
                }
                _ => {
                    let _ = tx.blocking_send(TaskEvent::Error(format!("{} API key not configured. Add it in Profile > AI Engine.", display_name)));
                }
            }
            let _ = tx.blocking_send(TaskEvent::Done { exit_code: 0 });
            return;
        }

        let final_prompt = if engine.is_code_task {
            compose_code_task_prompt(&prompt, session_memory.as_deref(), &project_path)
        } else {
            prompt.clone()
        };

        let result = if use_wsl {
            let distro = wsl_distro.as_deref().unwrap_or("Ubuntu");
            let wsl_path = windows_to_wsl_path(&project_path);
            // Use bash positional parameters to avoid shell injection.
            // The script cd's to $1 then execs the remaining args as the command.
            // model_id and prompt are passed as argv entries, never interpolated.
            let mut wsl_args: Vec<String> = vec![
                "-d".to_string(), distro.to_string(),
                "--".to_string(),
                "bash".to_string(), "-c".to_string(),
                r#"cd "$1" && shift && exec "$@""#.to_string(),
                "_".to_string(),      // $0 placeholder
                wsl_path.clone(),     // $1 = directory
                "claude".to_string(),
                "--print".to_string(),
                // Required: claude --print does not grant tool permissions automatically.
                // Without this flag, every tool call (Write, Bash, etc.) requires interactive
                // approval that can never arrive in a headless subprocess — tasks would hang
                // or produce no output. The agent's validate_git_command / validate_generated_file
                // / capability checks are the actual security boundary here.
                "--dangerously-skip-permissions".to_string(),
                "--verbose".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
            ];
            if let Some(model) = engine.model_id.as_deref().filter(|m| !m.is_empty()) {
                wsl_args.push("--model".to_string());
                wsl_args.push(model.to_string());
            }
            wsl_args.push(final_prompt.clone());
            let mut wsl_cmd = Command::new("wsl");
            wsl_cmd.args(&wsl_args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(key) = engine.api_key.as_deref().filter(|k| !k.is_empty()) {
                wsl_cmd.env("ANTHROPIC_API_KEY", key);
            }
            for (k, v) in &extra_env {
                wsl_cmd.env(k, v);
            }
            wsl_cmd.spawn()
        } else {
            let model_str = engine.model_id.clone().unwrap_or_default();
            // --dangerously-skip-permissions is required: without it, claude --print hangs
            // waiting for interactive permission grants that never arrive in a headless
            // subprocess. Security is enforced by validate_git_command, validate_generated_file,
            // and the capability system — not by Claude's permission prompts.
            let mut claude_args: Vec<String> = vec![
                "--print".into(),
                "--dangerously-skip-permissions".into(),
                "--verbose".into(),
                "--output-format".into(),
                "stream-json".into(),
            ];
            if !model_str.is_empty() {
                claude_args.push("--model".into());
                claude_args.push(model_str.clone());
            }
            claude_args.push(final_prompt.clone());

            // Wrap in cmd /c to give Claude Code a proper interactive shell session.
            // Claude's internal shell runner calls CreateProcessAsUserW when spawning
            // bash subprocesses; this fails with ERROR_NO_SUCH_LOGON_SESSION (1312) if
            // the process was not started inside a real cmd session with a valid token.
            #[allow(unused_mut)]
            let mut cmd = Command::new("cmd");
            // Build: cmd /c ""<claude_path>" <args...>"
            // The outer quotes are required by cmd /c when the path contains spaces.
            let inner: String = std::iter::once(format!("\"{}\"", claude_path))
                .chain(claude_args.iter().map(|a| {
                    if a.contains(' ') { format!("\"{}\"", a) } else { a.clone() }
                }))
                .collect::<Vec<_>>()
                .join(" ");
            cmd.args(["/c", &format!("\"{}\"", inner)])
                .current_dir(&project_path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(key) = engine.api_key.as_deref().filter(|k| !k.is_empty()) {
                cmd.env("ANTHROPIC_API_KEY", key);
            }
            for (k, v) in &extra_env {
                cmd.env(k, v);
            }
            #[cfg(target_os = "windows")]
            cmd.creation_flags(CREATE_NO_WINDOW);
            cmd.spawn()
        };

        let mut child = match result {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.blocking_send(TaskEvent::Error(format!("Failed to start claude: {e}")));
                return;
            }
        };

        // Record the PID so a `cancel_task` can kill the whole process tree.
        set_task_pid(&task_id, child.id());

        // Stream stdout — parse stream-json events into readable status lines
        if let Some(stdout) = child.stdout.take() {
            let tx2 = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(l) if l.trim().is_empty() => continue,
                        Ok(l) => {
                            for msg in format_stream_events(&l) {
                                if !msg.trim().is_empty() {
                                    let _ = tx2.blocking_send(TaskEvent::Output { data: msg + "\n", stream: "stdout" });
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // Stream stderr as-is (process-level errors, not Claude output)
        if let Some(stderr) = child.stderr.take() {
            let tx2 = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    match line {
                        Ok(l) => {
                            let _ = tx2.blocking_send(TaskEvent::Output { data: l + "\n", stream: "stderr" });
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        let exit_code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        let _ = tx.blocking_send(TaskEvent::Done { exit_code });
    });

    rx
}

#[derive(serde::Deserialize)]
struct OllamaTaskResponse {
    summary: Option<String>,
    response: Option<String>,
    questions: Option<Vec<String>>,
    files: Option<Vec<OllamaTaskFile>>,
}

#[derive(serde::Deserialize)]
struct OllamaTaskFile {
    path: String,
    content: String,
}

/// Ollama code-editing mode: read project files → ask Ollama for edits → write in-place.
fn run_ollama_code_task(
    tx: &mpsc::Sender<TaskEvent>,
    project_path: &str,
    prompt: &str,
    session_memory: Option<&str>,
    engine: &TaskEngine,
    cancel: &AtomicBool,
) {
    if cancel.load(Ordering::SeqCst) { return; }
    let endpoint = engine.endpoint.clone().unwrap_or_else(|| "http://localhost:11434".to_string());
    if let Err(e) = validate_ollama_endpoint(&endpoint) {
        let _ = tx.blocking_send(TaskEvent::Error(e));
        return;
    }
    let model_id = engine.model_id.clone().unwrap_or_else(|| "llama3.2".to_string());

    let _ = tx.blocking_send(TaskEvent::Output { data: "Reading project files\n".to_string(), stream: "stdout" });

    if !is_ollama_running(&endpoint) {
        if let Err(e) = setup_ollama(&model_id, &endpoint) {
            let _ = tx.blocking_send(TaskEvent::Error(e));
            return;
        }
    }

    // Collect source files (capped at 40, skip large/binary files)
    let source_files = collect_code_files(project_path);
    if source_files.is_empty() {
        let _ = tx.blocking_send(TaskEvent::Output { data: "No source files found in project\n".to_string(), stream: "stdout" });
        return;
    }

    let file_listing: String = source_files.iter()
        .map(|f| format!("- {}", f.relative_path))
        .collect::<Vec<_>>()
        .join("\n");

    // Read up to 20 files to include as context (skip very large ones)
    let mut file_contents = String::new();
    let mut included = 0;
    for entry in &source_files {
        if included >= 20 { break; }
        let full = std::path::Path::new(project_path).join(&entry.relative_path);
        if let Ok(content) = std::fs::read_to_string(&full) {
            if content.len() < 60_000 {
                file_contents.push_str(&format!(
                    "=== {} ===\n{}\n\n",
                    entry.relative_path, content
                ));
                included += 1;
            }
        }
    }

    let system_prompt = format!(
        "You are a code editor. You will be given source files from a project and a task.\n\
Return STRICT JSON only — no markdown, no code fences, no extra text — with this shape:\n\
{{\"summary\":\"short description of what happened\",\"response\":\"optional direct answer for the user\",\"questions\":[],\"files\":[{{\"path\":\"relative/path/to/file\",\"content\":\"full new file content\"}}]}}\n\
Rules:\n\
- If the user is asking you to inspect, explain, review, verify, or answer something, prefer `response` with `files: []`.\n\
- Only include files you actually changed.\n\
- Use the exact same relative paths as shown in the file listing.\n\
- You may create new files by using a new relative path.\n\
- Do not include files that are unchanged.\n\
- All paths must be relative to the project root (no leading slashes).\n\
- Use `questions` only if you truly cannot continue without clarification.\n\
- Project root: {}\n\
- Project files:\n{}\n\
- Output ONLY the JSON object.",
        project_path.replace('\\', "/"),
        file_listing,
    );

    let task_prompt = compose_code_task_prompt(prompt, session_memory, project_path);
    let user_message = if file_contents.is_empty() {
        format!("Task: {}", task_prompt)
    } else {
        format!("Current file contents:\n\n{}\nTask: {}", file_contents, task_prompt)
    };

    let _ = tx.blocking_send(TaskEvent::Output { data: "Generating code changes\n".to_string(), stream: "stdout" });

    let payload = serde_json::json!({
        "model": model_id,
        "stream": false,
        "format": "json",
        "prompt": format!("{}\n\n{}", system_prompt, user_message),
    });

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.blocking_send(TaskEvent::Error(format!("Could not build HTTP client: {e}")));
            return;
        }
    };

    let response = match client
        .post(format!("{}/api/generate", endpoint.trim_end_matches('/')))
        .json(&payload)
        .send()
    {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.blocking_send(TaskEvent::Error(format!("Ollama request failed: {e}")));
            return;
        }
    };

    if !response.status().is_success() {
        let _ = tx.blocking_send(TaskEvent::Error(format!("Ollama returned {}", response.status())));
        return;
    }

    let body = match response.json::<serde_json::Value>() {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.blocking_send(TaskEvent::Error(format!("Could not parse Ollama response: {e}")));
            return;
        }
    };

    let response_text = match body["response"].as_str() {
        Some(t) => t.to_string(),
        None => {
            let _ = tx.blocking_send(TaskEvent::Error("Ollama did not return response text".to_string()));
            return;
        }
    };

    let clean = extract_json_object(&response_text);
    let normalized = normalize_ollama_json(clean);
    let result: OllamaTaskResponse = match serde_json::from_str(&normalized) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[ollama-code] JSON parse failed: {e}");
            let _ = tx.blocking_send(TaskEvent::Error(format!("Ollama returned unreadable output. Try rephrasing your request.")));
            return;
        }
    };

    if let Some(summary) = result.summary.as_deref() {
        let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", summary.trim()), stream: "stdout" });
    }

    if let Some(response) = result.response.as_deref() {
        let trimmed = response.trim();
        if !trimmed.is_empty() {
            let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", trimmed), stream: "stdout" });
        }
    }

    let questions = result.questions.unwrap_or_default();
    let files = result.files.unwrap_or_default();
    if !questions.is_empty() && files.is_empty() {
        for question in &questions {
            let trimmed = question.trim();
            if !trimmed.is_empty() {
                let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", trimmed), stream: "stdout" });
            }
        }
        return;
    }

    if files.is_empty() {
        return;
    }

    for file in &files {
        let sanitized = sanitize_code_path(&file.path, project_path);
        let full_path = std::path::Path::new(project_path).join(&sanitized);
        if let Err(reason) = validate_generated_file(&sanitized, &file.content) {
            let _ = tx.blocking_send(TaskEvent::Error(reason));
            return;
        }
        if let Some(parent) = full_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&full_path, &file.content) {
            let _ = tx.blocking_send(TaskEvent::Error(format!("Could not write {}: {e}", sanitized.display())));
            return;
        }
        // Emit the same format the Claude Code parser expects so the app tracks changed files
        let _ = tx.blocking_send(TaskEvent::Output {
            data: format!("[Edit] {}\n", sanitized.to_string_lossy().replace('\\', "/")),
            stream: "stdout",
        });
    }
}

/// Collect source files from the project for code context (excludes binaries, build artifacts, etc.)
fn collect_code_files(project_path: &str) -> Vec<ProjectFileEntry> {
    let base = std::path::PathBuf::from(project_path);
    let mut results = Vec::new();
    let _ = collect_code_files_inner(&base, &base, 0, &mut results);
    // Sort: shortest paths first (more likely to be root-level important files)
    results.sort_by(|a, b| a.relative_path.len().cmp(&b.relative_path.len()));
    results.truncate(40);
    results
}

fn collect_code_files_inner(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    results: &mut Vec<ProjectFileEntry>,
) -> Result<(), String> {
    use std::time::UNIX_EPOCH;
    if depth > 3 { return Ok(()); }
    let entries = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if should_skip_path(&name) { continue; }
        if path.is_dir() {
            let _ = collect_code_files_inner(root, &path, depth + 1, results);
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if !is_source_file(&ext) { continue; }
        let metadata = match entry.metadata() { Ok(m) => m, Err(_) => continue };
        if metadata.len() > 200 * 1024 { continue; } // skip files >200KB
        let relative_path = path.strip_prefix(root).unwrap_or(&path)
            .to_string_lossy().replace('\\', "/");
        let modified_at = metadata.modified().ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs().to_string()).unwrap_or_default();
        results.push(ProjectFileEntry {
            relative_path,
            name,
            extension: ext.clone(),
            size: metadata.len(),
            modified_at,
            is_likely_data: false,
        });
    }
    Ok(())
}

fn is_source_file(ext: &str) -> bool {
    matches!(ext,
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" |
        "py" | "rs" | "go" | "java" | "kt" | "swift" |
        "c" | "cpp" | "h" | "hpp" | "cs" |
        "html" | "css" | "scss" | "less" |
        "json" | "toml" | "yaml" | "yml" |
        "md" | "txt" | "sh" | "bash" | "zsh" | "fish" |
        "sql" | "graphql" | "proto" | "env"
    )
}

/// Sanitize a code edit path — must stay within project_path.
fn sanitize_code_path(path: &str, project_path: &str) -> std::path::PathBuf {
    let mut sanitized = std::path::PathBuf::new();
    for component in std::path::Path::new(path).components() {
        if let std::path::Component::Normal(part) = component {
            sanitized.push(part);
        }
    }
    if sanitized.as_os_str().is_empty() {
        sanitized.push("output.txt");
    }
    // Guard: ensure the resolved path stays within the project root using
    // Path::starts_with (component-aware, not string prefix) to prevent bypass.
    let project_root = std::path::Path::new(project_path);
    let resolved = project_root.join(&sanitized);
    if !resolved.starts_with(project_root) {
        sanitized = std::path::PathBuf::from("output.txt");
    }
    sanitized
}

fn run_ollama_task(
    tx: &mpsc::Sender<TaskEvent>,
    project_path: &str,
    prompt: &str,
    engine: &TaskEngine,
) {
    let endpoint = engine.endpoint.clone().unwrap_or_else(|| "http://localhost:11434".to_string());
    if let Err(e) = validate_ollama_endpoint(&endpoint) {
        let _ = tx.blocking_send(TaskEvent::Error(e));
        return;
    }
    let model_id = engine.model_id.clone().unwrap_or_else(|| "llama3.2".to_string());
    let _ = tx.blocking_send(TaskEvent::Output { data: "Setting up workspace\n".to_string(), stream: "stdout" });

    if !is_ollama_running(&endpoint) {
        if let Err(error) = setup_ollama(&model_id, &endpoint) {
            let _ = tx.blocking_send(TaskEvent::Error(error));
            return;
        }
    }

    let _ = tx.blocking_send(TaskEvent::Output { data: "Working on your request\n".to_string(), stream: "stdout" });
    let output_root = std::path::Path::new(project_path).join("outpost-output");
    let _ = std::fs::create_dir_all(&output_root);

    match request_ollama_files(project_path, prompt, &model_id, &endpoint) {
        Ok(result) => {
            if let Some(summary) = result.summary.as_deref() {
                let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", summary.trim()), stream: "stdout" });
            }

            let files = result.files.unwrap_or_default();

            // Show clarifying questions only when there are truly no files to deliver
            let questions = result.questions.unwrap_or_default();
            if !questions.is_empty() && files.is_empty() {
                for question in &questions {
                    let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", question.trim()), stream: "stdout" });
                }
                return;
            }

            if files.is_empty() {
                return;
            }

            for file in files {
                let sanitized = sanitize_output_path(&file.path);
                let full_path = output_root.join(&sanitized);
                if let Err(reason) = validate_generated_file(&sanitized, &file.content) {
                    let _ = tx.blocking_send(TaskEvent::Error(reason));
                    return;
                }
                if let Some(parent) = full_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(error) = std::fs::write(&full_path, &file.content) {
                    let _ = tx.blocking_send(TaskEvent::Error(format!("Could not write {}: {}", sanitized.display(), error)));
                    return;
                }
                let _ = tx.blocking_send(TaskEvent::Output {
                    data: format!("Created {}\n", sanitized.to_string_lossy()),
                    stream: "stdout",
                });
            }
        }
        Err(error) => {
            eprintln!("[ollama] task error: {error}");
            let _ = tx.blocking_send(TaskEvent::Error(error));
        }
    }
}

/// Normalize Ollama JSON response to ensure `files` is always an array.
/// LLaMA sometimes returns `"files": {}` (empty object) or `"files": {"path": "content"}` (object map)
/// instead of the required `"files": []` array of `{path, content}` objects.
fn normalize_ollama_json(json: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
    };
    if let Some(obj) = value.as_object_mut() {
        if let Some(files_val) = obj.get("files") {
            match files_val {
                // Already an array — keep as-is
                serde_json::Value::Array(_) => {}
                // Empty object or non-array — replace with empty array
                serde_json::Value::Object(map) if map.is_empty() => {
                    obj.insert("files".to_string(), serde_json::Value::Array(vec![]));
                }
                // Object with entries like {"path": "content"} — convert to array form
                serde_json::Value::Object(map) => {
                    let arr: Vec<serde_json::Value> = map.iter().map(|(k, v)| {
                        serde_json::json!({
                            "path": k,
                            "content": v.as_str().unwrap_or("")
                        })
                    }).collect();
                    obj.insert("files".to_string(), serde_json::Value::Array(arr));
                }
                // Null or other type — replace with empty array
                _ => {
                    obj.insert("files".to_string(), serde_json::Value::Array(vec![]));
                }
            }
        }
    }
    value.to_string()
}

fn extract_json_object(text: &str) -> &str {
    let text = text.trim();
    // Strip ```json ... ``` or ``` ... ``` markdown code fences
    for fence in &["```json", "```"] {
        if let Some(start) = text.find(fence) {
            let after = &text[start + fence.len()..];
            let after = after.trim_start_matches('\n');
            if let Some(end) = after.find("```") {
                return after[..end].trim();
            }
        }
    }
    // Fall back to first { ... } block
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) {
        if end > start {
            return &text[start..=end];
        }
    }
    text
}

fn request_ollama_files(
    project_path: &str,
    prompt: &str,
    model_id: &str,
    endpoint: &str,
) -> Result<OllamaTaskResponse, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|e| format!("Could not initialize Ollama client: {e}"))?;

    let system_prompt = format!(
        "You are Outpost's local workspace engine.\n\
Return STRICT JSON only — no markdown, no code fences, no extra text — with this shape:\n\
{{\"summary\":\"short status\",\"questions\":[],\"files\":[{{\"path\":\"filename.ext\",\"content\":\"full file contents\"}}]}}\n\
Rules:\n\
- Produce files only when the task explicitly asks for a document, report, script, or other concrete deliverable.\n\
- If the user is asking a question, requesting an explanation, or asking you to analyze/inspect something, answer in `summary` with `files: []`.\n\
- If clarification is genuinely required, put questions in `questions` and set `files` to [].\n\
- All file paths must be relative file names or relative subpaths only.\n\
- Do not reference absolute paths.\n\
- The files will be written inside the project's outpost-output folder.\n\
- Use short, descriptive filenames (e.g. Quarterly_Sales_Report.pdf, Marketing_Plan.md). Base name must be under 40 characters. No timestamps or verbose descriptions.\n\
- Current workspace folder: {}\n\
- Output ONLY the JSON object. Do not wrap it in code fences.",
        project_path.replace('\\', "/"),
    );

    let payload = json!({
        "model": model_id,
        "stream": false,
        "format": "json",
        "prompt": format!("{}\n\nUser request:\n{}", system_prompt, prompt),
    });

    let response = client
        .post(format!("{}/api/generate", endpoint.trim_end_matches('/')))
        .json(&payload)
        .send()
        .map_err(|e| format!("Ollama request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("Ollama returned {}", response.status()));
    }

    let body = response
        .json::<serde_json::Value>()
        .map_err(|e| format!("Could not parse Ollama response: {e}"))?;
    let response_text = body["response"]
        .as_str()
        .ok_or("Ollama did not return response text")?;

    let clean = extract_json_object(response_text);
    let normalized = normalize_ollama_json(clean);
    serde_json::from_str::<OllamaTaskResponse>(&normalized).map_err(|e| {
        let preview: String = normalized.chars().take(300).collect();
        eprintln!("[ollama] JSON parse failed: {e}\nResponse preview: {preview}");
        format!("Ollama returned unreadable output (JSON parse error). Try rephrasing your request.")
    })
}

fn gemini_generate(api_key: &str, model: &str, prompt: &str, json_mode: bool) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| format!("Could not build HTTP client: {e}"))?;

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        model, api_key
    );

    let mut generation_config = serde_json::json!({});
    if json_mode {
        generation_config["responseMimeType"] = serde_json::json!("application/json");
    }

    let payload = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": prompt}]}],
        "generationConfig": generation_config,
    });

    let response = client
        .post(&url)
        .json(&payload)
        .send()
        .map_err(|e| format!("Gemini request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(format!("Gemini returned {status}: {body}"));
    }

    let body: serde_json::Value = response
        .json()
        .map_err(|e| format!("Could not parse Gemini response: {e}"))?;

    body["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("Gemini response missing text: {body}"))
}

fn run_gemini_task(
    tx: &mpsc::Sender<TaskEvent>,
    project_path: &str,
    prompt: &str,
    engine: &TaskEngine,
    api_key: &str,
) {
    let model = engine.model_id.clone().unwrap_or_else(|| "gemini-2.0-flash".to_string());
    let _ = tx.blocking_send(TaskEvent::Output { data: "Working on your request\n".to_string(), stream: "stdout" });
    let output_root = std::path::Path::new(project_path).join("outpost-output");
    let _ = std::fs::create_dir_all(&output_root);

    let system_prompt = format!(
        "You are Outpost's AI assistant.\n\
Return STRICT JSON only — no markdown, no code fences, no extra text — with this shape:\n\
{{\"summary\":\"short status\",\"questions\":[],\"files\":[{{\"path\":\"filename.ext\",\"content\":\"full file contents\"}}]}}\n\
Rules:\n\
- Produce files only when the task explicitly asks for a document, report, script, or other concrete deliverable.\n\
- If the user is asking a question, answer in `summary` with `files: []`.\n\
- All file paths must be relative file names only.\n\
- Use short, descriptive filenames (e.g. Quarterly_Sales_Report.pdf, Marketing_Plan.md). Base name must be under 40 characters. No timestamps or verbose descriptions.\n\
- Current workspace: {}\n\
- Output ONLY the JSON object.",
        project_path.replace('\\', "/"),
    );

    let full_prompt = format!("{}\n\nUser request:\n{}", system_prompt, prompt);

    match gemini_generate(api_key, &model, &full_prompt, true) {
        Ok(text) => {
            let clean = extract_json_object(&text);
            let normalized = normalize_ollama_json(clean);
            match serde_json::from_str::<OllamaTaskResponse>(&normalized) {
                Ok(result) => {
                    if let Some(summary) = result.summary.as_deref() {
                        let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", summary.trim()), stream: "stdout" });
                    }
                    let questions = result.questions.unwrap_or_default();
                    let files = result.files.unwrap_or_default();
                    if !questions.is_empty() && files.is_empty() {
                        for q in &questions {
                            let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", q.trim()), stream: "stdout" });
                        }
                        return;
                    }
                    for file in files {
                        let sanitized = sanitize_output_path(&file.path);
                        let full_path = output_root.join(&sanitized);
                        if let Err(reason) = validate_generated_file(&sanitized, &file.content) {
                            let _ = tx.blocking_send(TaskEvent::Error(reason));
                            return;
                        }
                        if let Some(parent) = full_path.parent() { let _ = std::fs::create_dir_all(parent); }
                        if let Err(e) = std::fs::write(&full_path, &file.content) {
                            let _ = tx.blocking_send(TaskEvent::Error(format!("Could not write {}: {e}", sanitized.display())));
                            return;
                        }
                        let _ = tx.blocking_send(TaskEvent::Output { data: format!("Created {}\n", sanitized.to_string_lossy()), stream: "stdout" });
                    }
                }
                Err(_) => {
                    // Non-JSON or plain text response — just show it
                    let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", text.trim()), stream: "stdout" });
                }
            }
        }
        Err(e) => {
            let _ = tx.blocking_send(TaskEvent::Error(e));
        }
    }
}

fn run_gemini_code_task(
    tx: &mpsc::Sender<TaskEvent>,
    project_path: &str,
    prompt: &str,
    session_memory: Option<&str>,
    engine: &TaskEngine,
    api_key: &str,
    cancel: &AtomicBool,
) {
    if cancel.load(Ordering::SeqCst) { return; }
    let model = engine.model_id.clone().unwrap_or_else(|| "gemini-2.0-flash".to_string());
    let _ = tx.blocking_send(TaskEvent::Output { data: "Reading project files\n".to_string(), stream: "stdout" });

    let source_files = collect_code_files(project_path);
    if source_files.is_empty() {
        let _ = tx.blocking_send(TaskEvent::Output { data: "No source files found in project\n".to_string(), stream: "stdout" });
        return;
    }

    let file_listing: String = source_files.iter()
        .map(|f| format!("- {}", f.relative_path))
        .collect::<Vec<_>>()
        .join("\n");

    let mut file_contents = String::new();
    let mut included = 0;
    for entry in &source_files {
        if included >= 20 { break; }
        let full = std::path::Path::new(project_path).join(&entry.relative_path);
        if let Ok(content) = std::fs::read_to_string(&full) {
            if content.len() < 60_000 {
                file_contents.push_str(&format!("=== {} ===\n{}\n\n", entry.relative_path, content));
                included += 1;
            }
        }
    }

    let system_prompt = format!(
        "You are a code editor. Return STRICT JSON only with this shape:\n\
{{\"summary\":\"short description\",\"response\":\"optional direct answer\",\"questions\":[],\"files\":[{{\"path\":\"relative/path\",\"content\":\"full new file content\"}}]}}\n\
Rules:\n\
- For questions/explanations, use `response` with `files: []`.\n\
- Only include files you actually changed.\n\
- Use exact relative paths as shown in the file listing.\n\
- Project root: {}\n\
- Project files:\n{}\n\
- Output ONLY the JSON object.",
        project_path.replace('\\', "/"),
        file_listing,
    );

    let task_prompt = compose_code_task_prompt(prompt, session_memory, project_path);
    let user_message = if file_contents.is_empty() {
        format!("Task: {}", task_prompt)
    } else {
        format!("Current file contents:\n\n{}\nTask: {}", file_contents, task_prompt)
    };

    let full_prompt = format!("{}\n\n{}", system_prompt, user_message);

    let _ = tx.blocking_send(TaskEvent::Output { data: "Generating code changes\n".to_string(), stream: "stdout" });

    match gemini_generate(api_key, &model, &full_prompt, true) {
        Ok(text) => {
            let clean = extract_json_object(&text);
            let normalized = normalize_ollama_json(clean);
            match serde_json::from_str::<OllamaTaskResponse>(&normalized) {
                Ok(result) => {
                    if let Some(summary) = result.summary.as_deref() {
                        let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", summary.trim()), stream: "stdout" });
                    }
                    if let Some(response) = result.response.as_deref() {
                        let trimmed = response.trim();
                        if !trimmed.is_empty() {
                            let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", trimmed), stream: "stdout" });
                        }
                    }
                    let questions = result.questions.unwrap_or_default();
                    let files = result.files.unwrap_or_default();
                    if !questions.is_empty() && files.is_empty() {
                        for q in &questions {
                            let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", q.trim()), stream: "stdout" });
                        }
                        return;
                    }
                    for file in &files {
                        let sanitized = sanitize_code_path(&file.path, project_path);
                        let full_path = std::path::Path::new(project_path).join(&sanitized);
                        if let Err(reason) = validate_generated_file(&sanitized, &file.content) {
                            let _ = tx.blocking_send(TaskEvent::Error(reason));
                            return;
                        }
                        if let Some(parent) = full_path.parent() { let _ = std::fs::create_dir_all(parent); }
                        if let Err(e) = std::fs::write(&full_path, &file.content) {
                            let _ = tx.blocking_send(TaskEvent::Error(format!("Could not write {}: {e}", sanitized.display())));
                            return;
                        }
                        let _ = tx.blocking_send(TaskEvent::Output {
                            data: format!("[Edit] {}\n", sanitized.to_string_lossy().replace('\\', "/")),
                            stream: "stdout",
                        });
                    }
                }
                Err(_) => {
                    let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", text.trim()), stream: "stdout" });
                }
            }
        }
        Err(e) => {
            let _ = tx.blocking_send(TaskEvent::Error(e));
        }
    }
}

// ── OpenAI-compatible providers (OpenAI, Groq, Mistral) ──────────────────────

fn openai_compat_generate(
    api_key: &str,
    base_url: &str,
    model: &str,
    system: &str,
    user: &str,
    json_mode: bool,
) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| format!("Could not build HTTP client: {e}"))?;

    let mut payload = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user",   "content": user},
        ],
    });
    if json_mode {
        payload["response_format"] = serde_json::json!({ "type": "json_object" });
    }

    let response = client
        .post(format!("{}/chat/completions", base_url.trim_end_matches('/')))
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .map_err(|e| format!("API request failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(format!("API returned {status}: {body}"));
    }

    let body: serde_json::Value = response
        .json()
        .map_err(|e| format!("Could not parse API response: {e}"))?;

    body["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("API response missing content: {body}"))
}

fn run_openai_compat_task(
    tx: &mpsc::Sender<TaskEvent>,
    project_path: &str,
    prompt: &str,
    engine: &TaskEngine,
    api_key: &str,
    base_url: &str,
    default_model: &str,
) {
    let model = engine.model_id.clone().unwrap_or_else(|| default_model.to_string());
    let _ = tx.blocking_send(TaskEvent::Output { data: "Working on your request\n".to_string(), stream: "stdout" });
    let output_root = std::path::Path::new(project_path).join("outpost-output");
    let _ = std::fs::create_dir_all(&output_root);

    let system_prompt = format!(
        "You are Outpost's AI assistant.\n\
Return STRICT JSON only — no markdown, no code fences, no extra text — with this shape:\n\
{{\"summary\":\"short status\",\"questions\":[],\"files\":[{{\"path\":\"filename.ext\",\"content\":\"full file contents\"}}]}}\n\
Rules:\n\
- Produce files only when the task explicitly asks for a document, report, script, or other concrete deliverable.\n\
- If the user is asking a question, answer in `summary` with `files: []`.\n\
- All file paths must be relative file names only.\n\
- Use short, descriptive filenames (e.g. Quarterly_Sales_Report.pdf, Marketing_Plan.md). Base name must be under 40 characters. No timestamps or verbose descriptions.\n\
- Current workspace: {}\n\
- Output ONLY the JSON object.",
        project_path.replace('\\', "/"),
    );

    match openai_compat_generate(api_key, base_url, &model, &system_prompt, prompt, true) {
        Ok(text) => {
            let clean = extract_json_object(&text);
            let normalized = normalize_ollama_json(clean);
            match serde_json::from_str::<OllamaTaskResponse>(&normalized) {
                Ok(result) => {
                    if let Some(summary) = result.summary.as_deref() {
                        let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", summary.trim()), stream: "stdout" });
                    }
                    let questions = result.questions.unwrap_or_default();
                    let files = result.files.unwrap_or_default();
                    if !questions.is_empty() && files.is_empty() {
                        for q in &questions {
                            let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", q.trim()), stream: "stdout" });
                        }
                        return;
                    }
                    for file in files {
                        let sanitized = sanitize_output_path(&file.path);
                        let full_path = output_root.join(&sanitized);
                        if let Err(reason) = validate_generated_file(&sanitized, &file.content) {
                            let _ = tx.blocking_send(TaskEvent::Error(reason));
                            return;
                        }
                        if let Some(parent) = full_path.parent() { let _ = std::fs::create_dir_all(parent); }
                        if let Err(e) = std::fs::write(&full_path, &file.content) {
                            let _ = tx.blocking_send(TaskEvent::Error(format!("Could not write {}: {e}", sanitized.display())));
                            return;
                        }
                        let _ = tx.blocking_send(TaskEvent::Output { data: format!("Created {}\n", sanitized.to_string_lossy()), stream: "stdout" });
                    }
                }
                Err(_) => {
                    let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", text.trim()), stream: "stdout" });
                }
            }
        }
        Err(e) => { let _ = tx.blocking_send(TaskEvent::Error(e)); }
    }
}

fn run_openai_compat_code_task(
    tx: &mpsc::Sender<TaskEvent>,
    project_path: &str,
    prompt: &str,
    session_memory: Option<&str>,
    engine: &TaskEngine,
    api_key: &str,
    base_url: &str,
    default_model: &str,
    cancel: &AtomicBool,
) {
    if cancel.load(Ordering::SeqCst) { return; }
    let model = engine.model_id.clone().unwrap_or_else(|| default_model.to_string());
    let _ = tx.blocking_send(TaskEvent::Output { data: "Reading project files\n".to_string(), stream: "stdout" });

    let source_files = collect_code_files(project_path);
    if source_files.is_empty() {
        let _ = tx.blocking_send(TaskEvent::Output { data: "No source files found in project\n".to_string(), stream: "stdout" });
        return;
    }

    let file_listing: String = source_files.iter()
        .map(|f| format!("- {}", f.relative_path))
        .collect::<Vec<_>>()
        .join("\n");

    let mut file_contents = String::new();
    let mut included = 0;
    for entry in &source_files {
        if included >= 20 { break; }
        let full = std::path::Path::new(project_path).join(&entry.relative_path);
        if let Ok(content) = std::fs::read_to_string(&full) {
            if content.len() < 60_000 {
                file_contents.push_str(&format!("=== {} ===\n{}\n\n", entry.relative_path, content));
                included += 1;
            }
        }
    }

    let system_prompt = format!(
        "You are a code editor. Return STRICT JSON only with this shape:\n\
{{\"summary\":\"short description\",\"response\":\"optional direct answer\",\"questions\":[],\"files\":[{{\"path\":\"relative/path\",\"content\":\"full new file content\"}}]}}\n\
Rules:\n\
- For questions/explanations, use `response` with `files: []`.\n\
- Only include files you actually changed.\n\
- Use exact relative paths as shown in the file listing.\n\
- Project root: {}\n\
- Project files:\n{}\n\
- Output ONLY the JSON object.",
        project_path.replace('\\', "/"),
        file_listing,
    );

    let task_prompt = compose_code_task_prompt(prompt, session_memory, project_path);
    let user_message = if file_contents.is_empty() {
        format!("Task: {}", task_prompt)
    } else {
        format!("Current file contents:\n\n{}\nTask: {}", file_contents, task_prompt)
    };

    let _ = tx.blocking_send(TaskEvent::Output { data: "Generating code changes\n".to_string(), stream: "stdout" });

    match openai_compat_generate(api_key, base_url, &model, &system_prompt, &user_message, true) {
        Ok(text) => {
            let clean = extract_json_object(&text);
            let normalized = normalize_ollama_json(clean);
            match serde_json::from_str::<OllamaTaskResponse>(&normalized) {
                Ok(result) => {
                    if let Some(summary) = result.summary.as_deref() {
                        let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", summary.trim()), stream: "stdout" });
                    }
                    if let Some(response) = result.response.as_deref() {
                        let trimmed = response.trim();
                        if !trimmed.is_empty() {
                            let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", trimmed), stream: "stdout" });
                        }
                    }
                    let questions = result.questions.unwrap_or_default();
                    let files = result.files.unwrap_or_default();
                    if !questions.is_empty() && files.is_empty() {
                        for q in &questions {
                            let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", q.trim()), stream: "stdout" });
                        }
                        return;
                    }
                    for file in &files {
                        let sanitized = sanitize_code_path(&file.path, project_path);
                        let full_path = std::path::Path::new(project_path).join(&sanitized);
                        if let Err(reason) = validate_generated_file(&sanitized, &file.content) {
                            let _ = tx.blocking_send(TaskEvent::Error(reason));
                            return;
                        }
                        if let Some(parent) = full_path.parent() { let _ = std::fs::create_dir_all(parent); }
                        if let Err(e) = std::fs::write(&full_path, &file.content) {
                            let _ = tx.blocking_send(TaskEvent::Error(format!("Could not write {}: {e}", sanitized.display())));
                            return;
                        }
                        let _ = tx.blocking_send(TaskEvent::Output {
                            data: format!("[Edit] {}\n", sanitized.to_string_lossy().replace('\\', "/")),
                            stream: "stdout",
                        });
                    }
                }
                Err(_) => {
                    let _ = tx.blocking_send(TaskEvent::Output { data: format!("{}\n", text.trim()), stream: "stdout" });
                }
            }
        }
        Err(e) => { let _ = tx.blocking_send(TaskEvent::Error(e)); }
    }
}

fn sanitize_output_path(path: &str) -> std::path::PathBuf {
    let mut sanitized = std::path::PathBuf::new();
    for component in std::path::Path::new(path).components() {
        if let std::path::Component::Normal(part) = component {
            sanitized.push(part);
        }
    }
    if sanitized.as_os_str().is_empty() {
        sanitized.push("deliverable.txt");
    }
    sanitized
}

fn validate_generated_file(path: &std::path::Path, content: &str) -> Result<(), String> {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("generated file")
        .to_string();
    let lower = content.to_lowercase();

    // Unambiguously malicious execution/download patterns only.
    // Do NOT add generic subprocess primitives (os.system, subprocess.popen,
    // child_process.exec) — those are standard library calls used in countless
    // legitimate scripts and would produce constant false positives.
    const DANGEROUS_PATTERNS: &[(&str, &str)] = &[
        ("curl|bash",          "detected pipe-to-shell downloader"),
        ("curl | bash",        "detected pipe-to-shell downloader"),
        ("wget|bash",          "detected pipe-to-shell downloader"),
        ("wget | bash",        "detected pipe-to-shell downloader"),
        ("invoke-expression",  "detected PowerShell dynamic execution"),
        ("iex(",               "detected PowerShell dynamic execution"),
        ("powershell -enc",    "detected encoded PowerShell execution"),
        ("frombase64string(",  "detected possible obfuscated payload decode"),
    ];

    for (needle, reason) in DANGEROUS_PATTERNS {
        if lower.contains(needle) {
            return Err(format!(
                "Blocked write for {}: {}",
                filename, reason
            ));
        }
    }

    // Block credential-harvesting path targets in generated code.
    // Keep this list specific — broad markers like ".config/" produce false positives
    // on any config-directory reference in legitimate code.
    const SENSITIVE_PATH_MARKERS: &[&str] = &[
        ".ssh/",
        ".aws/credentials",
        "appdata/local/google/chrome/user data",
        "appdata/roaming/mozilla/firefox/profiles",
        "/etc/shadow",
        "/etc/passwd",
    ];
    for marker in SENSITIVE_PATH_MARKERS {
        if lower.contains(marker) {
            return Err(format!(
                "Blocked write for {}: references sensitive credential path ({})",
                filename, marker
            ));
        }
    }

    Ok(())
}

/// Parse a stream-json event line into zero or more human-readable transcript
/// steps for the phone UI. Each returned string is one logical step (assistant
/// prose, a thinking block, a tool call, or a tool result) so the phone can
/// render them as distinct rows instead of one merged blob.
///
/// Tool calls keep the `[ToolName] summary` convention. Thinking blocks and tool
/// results use the `[Thinking]` / `[ToolResult]` / `[ToolError]` markers, which
/// the phone styles distinctly. Older phones that don't recognise the markers
/// degrade gracefully — they just render the text as a plain line.
fn format_stream_events(line: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };

    let mut parts: Vec<String> = Vec::new();
    match v["type"].as_str().unwrap_or("") {
        "assistant" => {
            let Some(content) = v["message"]["content"].as_array() else {
                return parts;
            };
            for item in content {
                match item["type"].as_str().unwrap_or("") {
                    "text" => {
                        let text = item["text"].as_str().unwrap_or("").trim();
                        if !text.is_empty() {
                            parts.push(text.to_string());
                        }
                    }
                    "thinking" => {
                        let text = item["thinking"].as_str().unwrap_or("").trim();
                        if !text.is_empty() {
                            parts.push(format!("[Thinking] {}", collapse_ws(text, 280)));
                        }
                    }
                    "tool_use" => {
                        let name = item["name"].as_str().unwrap_or("Tool");
                        let summary = tool_input_summary(name, &item["input"]);
                        parts.push(if summary.is_empty() {
                            format!("[{}]", name)
                        } else {
                            format!("[{}] {}", name, summary)
                        });
                    }
                    _ => {}
                }
            }
        }
        // Tool results come back as `user`-role messages in stream-json. These
        // carry what a command actually printed / what a read returned — the most
        // valuable "what is going on" signal, previously dropped entirely.
        "user" => {
            let Some(content) = v["message"]["content"].as_array() else {
                return parts;
            };
            for item in content {
                if item["type"].as_str().unwrap_or("") != "tool_result" {
                    continue;
                }
                let is_error = item["is_error"].as_bool().unwrap_or(false);
                let preview = collapse_ws(&extract_tool_result_text(&item["content"]), 300);
                if preview.is_empty() {
                    continue;
                }
                let marker = if is_error { "ToolError" } else { "ToolResult" };
                parts.push(format!("[{}] {}", marker, preview));
            }
        }
        // "result" is intentionally omitted — its text duplicates the last assistant event
        _ => {}
    }
    // Redact secrets from every transcript line before it leaves the agent.
    // Tool results (command output) and thinking blocks can contain tokens —
    // e.g. a task that runs `printenv` — and this stream is forwarded to the
    // relay/phone and persisted in chat history, so it must be scrubbed like
    // the prompt and session memory are.
    parts.into_iter().map(|p| redact_secrets(&p)).collect()
}

/// Flatten newlines/runs of whitespace into single spaces and truncate, so a
/// multi-line tool result or thinking block renders as a tidy one-line preview.
fn collapse_ws(s: &str, max_chars: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&collapsed, max_chars)
}

/// A tool_result's `content` is either a plain string or an array of content
/// blocks. Normalise both to a string: text blocks contribute their text,
/// non-text blocks (e.g. images) contribute a `[type]` placeholder so adjacent
/// text isn't silently concatenated across a dropped block.
fn extract_tool_result_text(content: &serde_json::Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(arr) = content.as_array() else {
        return String::new();
    };
    arr.iter()
        .filter_map(|item| {
            if let Some(t) = item["text"].as_str() {
                Some(t.to_string())
            } else {
                item["type"].as_str().map(|kind| format!("[{kind}]"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Summarize the input of a tool call for display in the phone UI.
fn tool_input_summary(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Bash" => truncate(input["command"].as_str().unwrap_or(""), 100),
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            input["file_path"].as_str().unwrap_or("").to_string()
        }
        "Read" => input["file_path"].as_str().unwrap_or("").to_string(),
        "Glob" => input["pattern"].as_str().unwrap_or("").to_string(),
        "Grep" => truncate(input["pattern"].as_str().unwrap_or(""), 60),
        "WebFetch" => truncate(input["url"].as_str().unwrap_or(""), 60),
        "WebSearch" => truncate(input["query"].as_str().unwrap_or(""), 60),
        "Task" | "Agent" => truncate(input["description"].as_str().unwrap_or(""), 60),
        _ => String::new(),
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}…", truncated)
    } else {
        truncated
    }
}

// ── Git diff collection (post-task) ──────────────────────────────────────────

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub path: String,
    pub status: String, // "modified" | "added" | "deleted"
    pub additions: i32,
    pub deletions: i32,
    pub patch: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitChanges {
    pub files: Vec<FileDiff>,
    pub is_git_repo: bool,
}

/// Run a git command for internal read-only use. Not routed through the relay
/// capability or validation system — only called from collect_git_changes.
/// Public alias used by ws_client for lightweight read-only git queries
/// (remote URL, current branch, etc.) outside the capability system.
pub fn run_git_readonly(args: &[&str], cwd: &str) -> Result<String, String> {
    run_raw_git(args, cwd)
}

fn run_raw_git(args: &[&str], cwd: &str) -> Result<String, String> {
    #[allow(unused_mut)]
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).current_dir(cwd);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let output = cmd.output().map_err(|e| format!("git {}: {e}", args[0]))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn get_file_patch_from_head(project_path: &str, file_path: &str) -> String {
    let full = std::path::Path::new(project_path).join(file_path);
    if !full.starts_with(project_path) { return String::new(); }
    let output = run_raw_git(&["diff", "HEAD", "--", file_path], project_path)
        .unwrap_or_default();
    const MAX_PATCH: usize = 4096;
    if output.len() > MAX_PATCH {
        let truncated: String = output.chars().take(MAX_PATCH).collect();
        format!("{}\n[… diff truncated]", truncated.trim_end())
    } else {
        output
    }
}

/// Collect git changes in the project after a task completes.
/// Returns `is_git_repo: false` for non-git directories. For git repos with no
/// staged/unstaged changes, returns an empty `files` list.
pub fn collect_git_changes(project_path: &str) -> GitChanges {
    if run_raw_git(&["rev-parse", "--git-dir"], project_path).is_err() {
        return GitChanges { files: vec![], is_git_repo: false };
    }
    // New repo with no commits yet — nothing to diff against
    if run_raw_git(&["rev-parse", "HEAD"], project_path).is_err() {
        return GitChanges { files: vec![], is_git_repo: true };
    }

    let numstat = run_raw_git(&["diff", "HEAD", "--numstat"], project_path).unwrap_or_default();
    let status_out = run_raw_git(&["status", "--porcelain"], project_path).unwrap_or_default();

    let mut files: Vec<FileDiff> = Vec::new();

    for line in numstat.lines().take(20) {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() < 3 { continue; }
        if parts[0] == "-" || parts[1] == "-" { continue; } // binary
        let additions: i32 = parts[0].parse().unwrap_or(0);
        let deletions: i32 = parts[1].parse().unwrap_or(0);
        let path = parts[2].trim().to_string();
        let status = if additions == 0 && deletions > 0 { "deleted" }
                     else if deletions == 0              { "added" }
                     else                                { "modified" };
        let patch = get_file_patch_from_head(project_path, &path);
        files.push(FileDiff { path, status: status.to_string(), additions, deletions, patch });
    }

    // Include untracked new files not already captured above
    for line in status_out.lines() {
        if !line.starts_with("?? ") { continue; }
        let path = line[3..].trim().trim_end_matches('/').to_string();
        if path.ends_with('/') { continue; }
        if files.iter().any(|f| f.path == path) { continue; }
        if files.len() >= 20 { break; }
        let full_path = std::path::Path::new(project_path).join(&path);
        if !full_path.is_file() { continue; }
        let content = std::fs::read_to_string(&full_path).unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let additions = lines.len() as i32;
        let patch: String = lines.iter().take(100).map(|l| format!("+{l}\n")).collect();
        files.push(FileDiff { path, status: "added".to_string(), additions, deletions: 0, patch });
    }

    GitChanges { files, is_git_repo: true }
}

/// Convert `C:\path\to\dir` → `/mnt/c/path/to/dir` for WSL
fn windows_to_wsl_path(path: &str) -> String {
    if path.len() >= 2 && &path[1..2] == ":" {
        let drive = path[0..1].to_lowercase();
        let rest = path[2..].replace('\\', "/");
        format!("/mnt/{drive}{rest}")
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_generated_file_allows_normal_content() {
        let path = std::path::Path::new("notes.md");
        let content = "Project summary and normal implementation details.";
        assert!(validate_generated_file(path, content).is_ok());
    }

    #[test]
    fn validate_generated_file_blocks_pipe_to_shell() {
        let path = std::path::Path::new("script.sh");
        let content = "curl|bash https://example.com/install.sh";
        assert!(validate_generated_file(path, content).is_err());
    }

    #[test]
    fn validate_generated_file_blocks_sensitive_path_reference() {
        let path = std::path::Path::new("stealer.py");
        let content = "open('/etc/shadow').read()";
        assert!(validate_generated_file(path, content).is_err());
    }

    #[test]
    fn validate_generated_file_allows_common_subprocess_calls() {
        let path = std::path::Path::new("script.py");
        let content = "import subprocess\nresult = subprocess.Popen(['ls', '-la'])\nimport os\nos.system('make build')";
        assert!(validate_generated_file(path, content).is_ok());
    }

    #[test]
    fn redact_secrets_masks_known_tokens() {
        let raw = "token sk-ant-api03-abc123 and jwt abcdefgh.ijklmnop.qrstuvwx";
        let redacted = redact_secrets(raw);
        assert!(!redacted.contains("sk-ant-api03-abc123"));
        assert!(redacted.contains("[REDACTED_SECRET]"));
        assert!(redacted.contains("[REDACTED_SECRET]") || redacted.contains("[REDACTED_JWT]"));
    }

    #[test]
    fn validate_git_policy_allows_expected_commands() {
        assert!(validate_git_command(&["add", "-A"]).is_ok());
        assert!(validate_git_command(&["commit", "-m", "msg"]).is_ok());
        assert!(validate_git_command(&["push"]).is_ok());
    }

    #[test]
    fn validate_git_policy_blocks_unexpected_commands() {
        assert!(validate_git_command(&["reset", "--hard"]).is_err());
        assert!(validate_git_command(&["push", "--force"]).is_err());
        assert!(validate_git_command(&["clean", "-fd"]).is_err());
    }

    #[test]
    fn stream_events_emits_thinking_and_tool_call_as_separate_steps() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"thinking","thinking":"Let me inspect the file first."},
            {"type":"tool_use","name":"Read","input":{"file_path":"src/main.rs"}}
        ]}}"#;
        let steps = format_stream_events(line);
        assert_eq!(steps.len(), 2);
        assert!(steps[0].starts_with("[Thinking] Let me inspect"));
        assert_eq!(steps[1], "[Read] src/main.rs");
    }

    #[test]
    fn stream_events_surfaces_tool_results_from_user_messages() {
        let ok = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","content":"line one\nline two"}
        ]}}"#;
        let steps = format_stream_events(ok);
        assert_eq!(steps, vec!["[ToolResult] line one line two".to_string()]);

        let err = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","is_error":true,
             "content":[{"type":"text","text":"command not found"}]}
        ]}}"#;
        let steps = format_stream_events(err);
        assert_eq!(steps, vec!["[ToolError] command not found".to_string()]);
    }

    #[test]
    fn stream_events_ignores_unrelated_event_types() {
        assert!(format_stream_events(r#"{"type":"result","subtype":"success"}"#).is_empty());
        assert!(format_stream_events("not json").is_empty());
    }

    #[test]
    fn stream_events_redacts_secrets_in_tool_results() {
        // A tool result that printed an env var with a token must be scrubbed
        // before it leaves the agent toward the relay/phone/history.
        let line = r#"{"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t1","content":"GITHUB_TOKEN=ghp_abcdefgh1234567890ABCD"}
        ]}}"#;
        let steps = format_stream_events(line);
        assert_eq!(steps.len(), 1);
        assert!(!steps[0].contains("ghp_abcdefgh1234567890ABCD"), "token leaked: {}", steps[0]);
        assert!(steps[0].contains("[REDACTED_SECRET]"));
    }
}
