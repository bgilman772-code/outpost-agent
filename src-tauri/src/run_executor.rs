// Runtime execution adapter for agent runs.
//
// `start_run` (dispatched from ws_client) builds a command from the selected
// runtime config, spawns it in the project directory, and streams stdout/stderr
// back to the relay as run events, finishing with a run status. Cancellation
// kills the process tree. The pure helpers (command building, stream-json
// parsing, status mapping) are unit-tested; the spawn path is integration-tested
// on a real machine.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::mpsc;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Events streamed while a run executes.
#[derive(Debug, Clone, PartialEq)]
pub enum RunEvent {
    /// A raw output line (stream is "stdout" or "stderr").
    Output { line: String, stream: &'static str },
    /// A human-readable progress step parsed from structured output.
    Step { text: String },
    /// Final summary parsed from the runtime's result event.
    Summary { text: String },
    /// Git diff of the run's changes (computed at run end).
    Diff {
        files: Vec<DiffFile>,
        additions: u32,
        deletions: u32,
        patch: String,
    },
    /// Process exited (terminal). `status` is the resolved AgentRunStatus
    /// ("completed" / "failed" / "canceled").
    Done {
        exit_code: i32,
        status: &'static str,
    },
    /// Could not start / fatal error (terminal).
    Failed { error: String },
}

// ── Pure helpers (unit-tested) ──────────────────────────────────────────────

/// Build the `(program, args)` to run for a runtime invocation. Every
/// `{{prompt}}` in the arg template is replaced with the prompt. An empty
/// command (custom CLI not configured, or outpost_legacy) is rejected.
pub fn build_invocation(
    command: &str,
    args_template: &[String],
    prompt: &str,
) -> Result<(String, Vec<String>), String> {
    let program = command.trim();
    if program.is_empty() {
        return Err("This runtime has no command configured.".to_string());
    }
    let args = args_template
        .iter()
        .map(|a| a.replace("{{prompt}}", prompt))
        .collect();
    Ok((program.to_string(), args))
}

/// Map a process exit code (and cancellation flag) to a run status string that
/// matches the relay's AgentRunStatus.
pub fn run_status_for(exit_code: i32, canceled: bool) -> &'static str {
    if canceled {
        "canceled"
    } else if exit_code == 0 {
        "completed"
    } else {
        "failed"
    }
}

/// Parsed signal extracted from a single line of structured runtime output.
#[derive(Debug, Default, PartialEq)]
pub struct ParsedLine {
    pub step: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

fn truncate(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        t.to_string()
    } else {
        let cut: String = t.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// Best-effort parse of one line of Claude Code `--output-format stream-json`.
/// Recognises `assistant` text deltas (→ step) and the terminal `result` event
/// (→ summary or error). Non-JSON or unrecognised lines yield an empty result,
/// so plain-text runtimes degrade gracefully.
pub fn parse_stream_json_line(line: &str) -> ParsedLine {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return ParsedLine::default();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return ParsedLine::default();
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => {
            // message.content[] may hold { type:"text", text:"..." } blocks.
            let text = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            if text.trim().is_empty() {
                ParsedLine::default()
            } else {
                ParsedLine {
                    step: Some(truncate(&text, 140)),
                    ..Default::default()
                }
            }
        }
        Some("result") => {
            let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
            let result = v
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            if is_error {
                ParsedLine {
                    error: Some(if result.is_empty() {
                        "Runtime reported an error".to_string()
                    } else {
                        result
                    }),
                    ..Default::default()
                }
            } else {
                ParsedLine {
                    summary: Some(truncate(&result, 400)),
                    ..Default::default()
                }
            }
        }
        _ => ParsedLine::default(),
    }
}

// ── Cancellation registry ───────────────────────────────────────────────────

struct RunHandle {
    cancel: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    pid: Option<u32>,
}

static RUNS: OnceLock<Mutex<HashMap<String, RunHandle>>> = OnceLock::new();

fn runs() -> &'static Mutex<HashMap<String, RunHandle>> {
    RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Request cancellation of a running run: sets its flag and kills the process
/// tree. Returns true if the run was known.
pub fn cancel_run(run_id: &str) -> bool {
    let entry = runs()
        .lock()
        .unwrap()
        .get(run_id)
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

fn request_run_restart(run_id: &str) -> bool {
    let entry = runs()
        .lock()
        .unwrap()
        .get(run_id)
        .map(|h| (h.restart.clone(), h.pid));
    match entry {
        Some((restart, pid)) => {
            restart.store(true, Ordering::SeqCst);
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
        cmd.args(["/T", "/F", "/PID", &pid.to_string()]);
        cmd.creation_flags(CREATE_NO_WINDOW);
        let _ = cmd.spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .spawn();
        let pid_str = pid.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(3));
            let _ = Command::new("kill").args(["-KILL", &pid_str]).spawn();
        });
    }
}

// ── Git diff ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DiffFile {
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
}

pub struct DiffData {
    pub files: Vec<DiffFile>,
    pub additions: u32,
    pub deletions: u32,
    pub patch: String,
}

const MAX_PATCH_BYTES: usize = 200_000;

#[derive(Debug)]
pub struct GitBaseline {
    tree: String,
    index_path: std::path::PathBuf,
}

/// Parse `git diff --numstat` output: lines of `ADDS<TAB>DELS<TAB>path`.
/// Binary files report `-` for counts → treated as 0. Returns per-file stats
/// and the totals.
pub fn parse_numstat(text: &str) -> (Vec<DiffFile>, u32, u32) {
    let mut files = Vec::new();
    let mut total_add = 0u32;
    let mut total_del = 0u32;
    for line in text.lines() {
        let mut parts = line.split('\t');
        let add = parts.next().unwrap_or("").trim();
        let del = parts.next().unwrap_or("").trim();
        let path = parts.collect::<Vec<_>>().join("\t");
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        let additions = add.parse::<u32>().unwrap_or(0);
        let deletions = del.parse::<u32>().unwrap_or(0);
        total_add += additions;
        total_del += deletions;
        files.push(DiffFile {
            path: path.to_string(),
            additions,
            deletions,
        });
    }
    (files, total_add, total_del)
}

/// Redact obvious secrets that may appear in a diff line. Replaces the tail of a
/// recognised token with asterisks so the diff is safe to send to the phone.
pub fn redact_secret_line(line: &str) -> String {
    const PREFIXES: &[&str] = &[
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "sk-",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "ASIA",
    ];
    let mut out = line.to_string();
    for p in PREFIXES {
        let mut cursor = 0;
        while let Some(relative) = out[cursor..].find(p) {
            let idx = cursor + relative;
            // Replace from the prefix to the next whitespace/quote with redaction.
            let start = idx + p.len();
            let end = out[start..]
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
                .map(|rel| start + rel)
                .unwrap_or(out.len());
            if end > start {
                out.replace_range(start..end, "***REDACTED***");
            }
            cursor = start + "***REDACTED***".len();
            if cursor >= out.len() {
                break;
            }
        }
    }
    if out.contains("PRIVATE KEY-----") {
        // Don't ship private key bodies; keep just the marker line.
        if out.trim_start_matches(['+', '-', ' ']).starts_with("-----") {
            // header/footer line is fine
        } else {
            out = format!("{} ***REDACTED PRIVATE KEY***", &out[..out.len().min(1)]);
        }
    }
    out
}

fn redact_and_truncate(patch: &str) -> String {
    let mut in_private_key = false;
    let redacted = patch
        .lines()
        .map(|line| {
            if line.contains("BEGIN ") && line.contains("PRIVATE KEY") {
                in_private_key = true;
                return format!("{}***REDACTED PRIVATE KEY***", &line[..line.len().min(1)]);
            }
            if in_private_key {
                if line.contains("END ") && line.contains("PRIVATE KEY") {
                    in_private_key = false;
                }
                return format!("{}***REDACTED PRIVATE KEY***", &line[..line.len().min(1)]);
            }
            redact_secret_line(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if redacted.len() > MAX_PATCH_BYTES {
        let mut cut = MAX_PATCH_BYTES;
        while !redacted.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n… diff truncated …", &redacted[..cut])
    } else {
        redacted
    }
}

fn run_git_with_env(
    project_path: &str,
    args: &[&str],
    env: &[(&str, &std::ffi::OsStr)],
) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(project_path).args(args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    match cmd.output() {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).to_string()),
        _ => None,
    }
}

fn run_git(project_path: &str, args: &[&str]) -> Option<String> {
    run_git_with_env(project_path, args, &[])
}

fn snapshot_tree(project_path: &str, index_path: &std::path::Path) -> Option<String> {
    let index_value = index_path.as_os_str();
    let env = [("GIT_INDEX_FILE", index_value)];
    let _ = std::fs::remove_file(index_path);
    run_git_with_env(project_path, &["add", "-A"], &env)?;
    run_git_with_env(project_path, &["write-tree"], &env).map(|s| s.trim().to_string())
}

/// Capture the complete worktree state before a run without modifying the
/// user's real index. The temporary tree includes tracked and untracked files.
pub fn capture_git_baseline(project_path: &str, run_id: &str) -> Option<GitBaseline> {
    run_git(project_path, &["rev-parse", "--git-dir"])?;
    let safe_id: String = run_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let index_path = std::env::temp_dir().join(format!("outpost-{safe_id}.index"));
    let tree = snapshot_tree(project_path, &index_path)?;
    Some(GitBaseline { tree, index_path })
}

/// Compute only changes made after the captured run baseline. This excludes
/// dirty work that existed before the run and includes newly created files.
pub fn compute_git_diff(project_path: &str, baseline: &GitBaseline) -> Option<DiffData> {
    let after_tree = snapshot_tree(project_path, &baseline.index_path)?;
    let numstat = run_git(
        project_path,
        &["diff", &baseline.tree, &after_tree, "--numstat"],
    )?;
    let (files, additions, deletions) = parse_numstat(&numstat);
    if files.is_empty() {
        return None;
    }
    let patch_raw =
        run_git(project_path, &["diff", &baseline.tree, &after_tree]).unwrap_or_default();
    Some(DiffData {
        files,
        additions,
        deletions,
        patch: redact_and_truncate(&patch_raw),
    })
}

// ── Scoped secret grants ────────────────────────────────────────────────────
//
// Secrets the user approved for a specific run, delivered by the relay over the
// WS. Held only in memory, keyed by run id, and cleared when the run ends. The
// value is never logged. (Injecting a granted secret into an already-running
// runtime needs runtime-specific hooks; this is the receiving half.)

static SECRETS: OnceLock<Mutex<HashMap<String, HashMap<String, String>>>> = OnceLock::new();

fn secrets() -> &'static Mutex<HashMap<String, HashMap<String, String>>> {
    SECRETS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Store a secret granted to a run. Logs the NAME only, never the value.
pub fn store_secret_grant(run_id: &str, name: &str, value: &str) {
    secrets()
        .lock()
        .unwrap()
        .entry(run_id.to_string())
        .or_default()
        .insert(name.to_string(), value.to_string());
    request_run_restart(run_id);
}

/// Secrets currently granted to a run (name → value).
pub fn secret_grants_for(run_id: &str) -> HashMap<String, String> {
    secrets()
        .lock()
        .unwrap()
        .get(run_id)
        .cloned()
        .unwrap_or_default()
}

fn clear_secret_grants(run_id: &str) {
    secrets().lock().unwrap().remove(run_id);
}

/// Redact output before it leaves the desktop. Exact values for all secrets
/// granted to this run are removed in addition to common token patterns.
pub fn redact_run_output(run_id: &str, line: &str) -> String {
    let mut redacted = redact_secret_line(line);
    for value in secret_grants_for(run_id).values() {
        if !value.is_empty() {
            redacted = redacted.replace(value, "***REDACTED***");
        }
    }
    redacted
}

/// Apply the supervised Claude adapter's permission profile. Other runtimes are
/// rejected by the relay until they have equivalent enforceable controls.
pub fn apply_runtime_policy(
    runtime: &str,
    profile_id: &str,
    args: &mut Vec<String>,
) -> Result<(), String> {
    if runtime != "claude_code" {
        return Err(format!(
            "Runtime '{runtime}' does not yet support supervised execution"
        ));
    }
    let mode = match profile_id {
        "safe" => "plan",
        "autonomous" => "auto",
        _ => "acceptEdits",
    };
    args.extend(["--permission-mode".to_string(), mode.to_string()]);
    if profile_id != "safe" {
        args.push("--disallowedTools".to_string());
        args.extend(
            [
                "Bash(git push *)",
                "Bash(rm *)",
                "Bash(del *)",
                "Bash(Remove-Item *)",
                "Bash(kubectl apply *)",
                "Bash(terraform apply *)",
                "Bash(vercel deploy *)",
                "Bash(netlify deploy *)",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
    }
    Ok(())
}

fn apply_safe_inherited_env(cmd: &mut Command) {
    const SAFE_ENV_KEYS: &[&str] = &[
        "PATH",
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "PROGRAMDATA",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "TEMP",
        "TMP",
        "SHELL",
        "USER",
        "USERNAME",
        "LANG",
        "LC_ALL",
        "TERM",
    ];
    cmd.env_clear();
    for key in SAFE_ENV_KEYS {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
}

// ── Execution ───────────────────────────────────────────────────────────────

/// Spawn a run: validate the project path, start `program args` in it, and
/// stream RunEvents over the returned channel. The final event is always
/// `Done` or `Failed`. Env vars are applied to the child process.
pub fn spawn_run(
    run_id: String,
    project_path: String,
    program: String,
    args: Vec<String>,
    env: HashMap<String, String>,
) -> mpsc::Receiver<RunEvent> {
    let (tx, rx) = mpsc::channel::<RunEvent>(512);
    let cancel = Arc::new(AtomicBool::new(false));
    let restart = Arc::new(AtomicBool::new(false));
    runs().lock().unwrap().insert(
        run_id.clone(),
        RunHandle {
            cancel: cancel.clone(),
            restart: restart.clone(),
            pid: None,
        },
    );

    std::thread::spawn(move || {
        let finish = |tx: &mpsc::Sender<RunEvent>, ev: RunEvent| {
            let _ = tx.blocking_send(ev);
        };

        if !crate::task_runner::is_path_within_home(&project_path) {
            finish(
                &tx,
                RunEvent::Failed {
                    error: "Project path is not inside the home directory.".to_string(),
                },
            );
            runs().lock().unwrap().remove(&run_id);
            return;
        }

        let baseline = capture_git_baseline(&project_path, &run_id);
        let status = loop {
            let mut cmd = Command::new(&program);
            apply_safe_inherited_env(&mut cmd);
            cmd.args(&args)
                .current_dir(&project_path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .stdin(Stdio::null());
            for (k, v) in &env {
                cmd.env(k, v);
            }
            // Apply only secrets explicitly granted to this run. A mid-run grant
            // restarts the runtime so the new process inherits the approved value.
            for (k, v) in secret_grants_for(&run_id) {
                cmd.env(k, v);
            }
            #[cfg(target_os = "windows")]
            cmd.creation_flags(CREATE_NO_WINDOW);

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    finish(
                        &tx,
                        RunEvent::Failed {
                            error: format!("Could not start '{program}': {e}"),
                        },
                    );
                    runs().lock().unwrap().remove(&run_id);
                    return;
                }
            };

            if let Some(h) = runs().lock().unwrap().get_mut(&run_id) {
                h.pid = Some(child.id());
            }

            // Reader thread for stdout: stream lines + parse structured signals.
            let stdout = child.stdout.take();
            let tx_out = tx.clone();
            let stdout_handle = std::thread::spawn(move || {
                if let Some(out) = stdout {
                    let reader = BufReader::new(out);
                    for line in reader.lines().map_while(Result::ok) {
                        let parsed = parse_stream_json_line(&line);
                        if let Some(step) = parsed.step {
                            let _ = tx_out.blocking_send(RunEvent::Step { text: step });
                        }
                        if let Some(summary) = parsed.summary {
                            let _ = tx_out.blocking_send(RunEvent::Summary { text: summary });
                        }
                        if let Some(error) = parsed.error {
                            let _ = tx_out.blocking_send(RunEvent::Output {
                                line: error,
                                stream: "stderr",
                            });
                        }
                        let _ = tx_out.blocking_send(RunEvent::Output {
                            line,
                            stream: "stdout",
                        });
                    }
                }
            });

            // Reader thread for stderr.
            let stderr = child.stderr.take();
            let tx_err = tx.clone();
            let stderr_handle = std::thread::spawn(move || {
                if let Some(err) = stderr {
                    let reader = BufReader::new(err);
                    for line in reader.lines().map_while(Result::ok) {
                        let _ = tx_err.blocking_send(RunEvent::Output {
                            line,
                            stream: "stderr",
                        });
                    }
                }
            });

            let status = child.wait();
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            if restart.swap(false, Ordering::SeqCst) && !cancel.load(Ordering::SeqCst) {
                let _ = tx.blocking_send(RunEvent::Step {
                    text: "Restarting runtime with approved secret access".to_string(),
                });
                continue;
            }
            break status;
        };
        let canceled = cancel.load(Ordering::SeqCst);

        // Compute the run's git diff and stream it before the terminal status, so
        // the phone can review changes. Skipped on cancel.
        if !canceled {
            if let Some(mut diff) = baseline
                .as_ref()
                .and_then(|snapshot| compute_git_diff(&project_path, snapshot))
            {
                diff.patch = diff
                    .patch
                    .lines()
                    .map(|line| redact_run_output(&run_id, line))
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = tx.blocking_send(RunEvent::Diff {
                    files: diff.files,
                    additions: diff.additions,
                    deletions: diff.deletions,
                    patch: diff.patch,
                });
            }
        }
        if let Some(snapshot) = baseline {
            let _ = std::fs::remove_file(snapshot.index_path);
        }

        runs().lock().unwrap().remove(&run_id);
        clear_secret_grants(&run_id);

        match status {
            Ok(s) => {
                let code = s.code().unwrap_or(if canceled { 130 } else { -1 });
                let final_code = if canceled { 130 } else { code };
                finish(
                    &tx,
                    RunEvent::Done {
                        exit_code: final_code,
                        status: run_status_for(final_code, canceled),
                    },
                );
            }
            Err(e) => finish(
                &tx,
                RunEvent::Failed {
                    error: format!("Run process error: {e}"),
                },
            ),
        }
    });

    rx
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_invocation_substitutes_prompt() {
        let tmpl = vec![
            "-p".to_string(),
            "{{prompt}}".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
        ];
        let (prog, args) = build_invocation("claude", &tmpl, "fix the tests").unwrap();
        assert_eq!(prog, "claude");
        assert_eq!(
            args,
            vec!["-p", "fix the tests", "--output-format", "stream-json"]
        );
    }

    #[test]
    fn build_invocation_rejects_empty_command() {
        assert!(build_invocation("", &[], "do a thing").is_err());
        assert!(build_invocation("   ", &[], "do a thing").is_err());
    }

    #[test]
    fn run_status_maps_exit_and_cancel() {
        assert_eq!(run_status_for(0, false), "completed");
        assert_eq!(run_status_for(1, false), "failed");
        assert_eq!(run_status_for(0, true), "canceled");
        assert_eq!(run_status_for(1, true), "canceled");
    }

    #[test]
    fn parse_plain_text_line_is_empty() {
        let p = parse_stream_json_line("hello from the agent");
        assert_eq!(p, ParsedLine::default());
    }

    #[test]
    fn parse_assistant_text_becomes_step() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reading the test files"}]}}"#;
        let p = parse_stream_json_line(line);
        assert_eq!(p.step.as_deref(), Some("Reading the test files"));
        assert!(p.summary.is_none() && p.error.is_none());
    }

    #[test]
    fn parse_result_becomes_summary() {
        let line = r#"{"type":"result","is_error":false,"result":"Fixed 3 failing tests."}"#;
        let p = parse_stream_json_line(line);
        assert_eq!(p.summary.as_deref(), Some("Fixed 3 failing tests."));
        assert!(p.error.is_none());
    }

    #[test]
    fn parse_error_result_becomes_error() {
        let line = r#"{"type":"result","is_error":true,"result":"rate limited"}"#;
        let p = parse_stream_json_line(line);
        assert_eq!(p.error.as_deref(), Some("rate limited"));
        assert!(p.summary.is_none());
    }

    #[test]
    fn parse_unknown_type_is_empty() {
        let line = r#"{"type":"system","subtype":"init"}"#;
        assert_eq!(parse_stream_json_line(line), ParsedLine::default());
    }

    #[test]
    fn cancel_unknown_run_returns_false() {
        assert!(!cancel_run("no-such-run"));
    }

    #[test]
    fn parse_numstat_reads_counts_and_totals() {
        let text = "3\t1\tsrc/a.ts\n10\t0\tsrc/b.ts\n-\t-\tlogo.png\n";
        let (files, add, del) = parse_numstat(text);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "src/a.ts");
        assert_eq!(files[0].additions, 3);
        assert_eq!(files[0].deletions, 1);
        assert_eq!(files[2].additions, 0); // binary
        assert_eq!(add, 13);
        assert_eq!(del, 1);
    }

    #[test]
    fn parse_numstat_empty_is_empty() {
        let (files, add, del) = parse_numstat("\n  \n");
        assert!(files.is_empty());
        assert_eq!((add, del), (0, 0));
    }

    #[test]
    fn redact_secret_line_masks_tokens() {
        assert!(
            redact_secret_line("+const t = \"ghp_abcdEFGH1234\"").contains("ghp_***REDACTED***")
        );
        assert!(!redact_secret_line("+const t = \"ghp_abcdEFGH1234\"").contains("abcdEFGH1234"));
        assert!(redact_secret_line("+key = sk-livesecret123").contains("sk-***REDACTED***"));
        let multiple = redact_secret_line("+a=ghp_first b=ghp_second");
        assert_eq!(multiple.matches("***REDACTED***").count(), 2);
        // Ordinary code is untouched.
        assert_eq!(redact_secret_line("+const x = 42;"), "+const x = 42;");
    }

    #[test]
    fn redact_patch_masks_private_key_body() {
        let patch = "+-----BEGIN PRIVATE KEY-----\n+super-secret-body\n+-----END PRIVATE KEY-----";
        let redacted = redact_and_truncate(patch);
        assert!(!redacted.contains("super-secret-body"));
        assert_eq!(redacted.matches("REDACTED PRIVATE KEY").count(), 3);
    }

    #[test]
    fn secret_grants_are_stored_and_cleared_per_run() {
        let run = "run-secret-test";
        assert!(secret_grants_for(run).is_empty());
        store_secret_grant(run, "GITHUB_TOKEN", "ghp_x");
        store_secret_grant(run, "VERCEL_TOKEN", "vc_y");
        let grants = secret_grants_for(run);
        assert_eq!(
            grants.get("GITHUB_TOKEN").map(String::as_str),
            Some("ghp_x")
        );
        assert_eq!(grants.len(), 2);
        // A different run sees nothing.
        assert!(secret_grants_for("other-run").is_empty());
        clear_secret_grants(run);
        assert!(secret_grants_for(run).is_empty());
    }

    #[test]
    fn runtime_policy_maps_profiles_and_keeps_hard_denies() {
        let mut safe = Vec::new();
        apply_runtime_policy("claude_code", "safe", &mut safe).unwrap();
        assert_eq!(safe, vec!["--permission-mode", "plan"]);

        let mut balanced = Vec::new();
        apply_runtime_policy("claude_code", "balanced", &mut balanced).unwrap();
        assert!(balanced
            .windows(2)
            .any(|w| w == ["--permission-mode", "acceptEdits"]));
        assert!(balanced.iter().any(|arg| arg == "Bash(git push *)"));
        assert!(apply_runtime_policy("codex", "balanced", &mut Vec::new()).is_err());
    }

    #[test]
    fn run_output_redacts_exact_granted_value() {
        let run = "run-output-secret";
        store_secret_grant(run, "CUSTOM_TOKEN", "totally-unknown-secret-format");
        let output = redact_run_output(run, "token=totally-unknown-secret-format");
        assert_eq!(output, "token=***REDACTED***");
        clear_secret_grants(run);
    }

    #[test]
    fn git_baseline_excludes_old_changes_and_includes_new_files() {
        let root = std::env::temp_dir().join(format!(
            "outpost-baseline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let project = root.to_string_lossy().to_string();
        assert!(Command::new("git")
            .args(["init", &project])
            .output()
            .unwrap()
            .status
            .success());
        std::fs::write(root.join("old.txt"), "already dirty\n").unwrap();
        let baseline = capture_git_baseline(&project, "baseline-test").unwrap();
        std::fs::write(root.join("old.txt"), "already dirty\nnew line\n").unwrap();
        std::fs::write(root.join("created.txt"), "created by run\n").unwrap();

        let diff = compute_git_diff(&project, &baseline).unwrap();
        assert_eq!(diff.files.len(), 2);
        assert!(diff.files.iter().any(|file| file.path == "created.txt"));
        assert!(diff.patch.contains("+new line"));
        assert!(!diff.patch.contains("+already dirty"));

        let _ = std::fs::remove_file(baseline.index_path);
        std::fs::remove_dir_all(root).ok();
    }
}
