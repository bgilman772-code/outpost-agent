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
    runs().lock().unwrap().insert(
        run_id.clone(),
        RunHandle {
            cancel: cancel.clone(),
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

        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .current_dir(&project_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        for (k, v) in &env {
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
        let canceled = cancel.load(Ordering::SeqCst);
        runs().lock().unwrap().remove(&run_id);

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
}
