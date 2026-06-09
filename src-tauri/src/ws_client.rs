use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, OnceLock};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    connect_async_tls_with_config, tungstenite::client::IntoClientRequest, Connector,
};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

macro_rules! wslog {
    ($($arg:tt)*) => {
        eprintln!("[ws] {}", format!($($arg)*));
    };
}

use crate::capabilities;
use crate::probe;
use crate::run_executor;
use crate::task_runner;

static CLAUDE_PATH: OnceLock<String> = OnceLock::new();

/// Upper bound on inbound relay messages dispatched concurrently per connection.
/// Generous enough that normal operation never blocks; low enough that a flood
/// can't spawn unbounded handler tasks.
const MAX_CONCURRENT_DISPATCH: usize = 32;

pub fn init_claude_path() {
    if CLAUDE_PATH.get().is_some() {
        return;
    }
    #[allow(unused_mut)]
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/C", "where", "claude"]);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    if let Ok(out) = cmd.output() {
        if out.status.success() {
            let cmd_path = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !cmd_path.is_empty() {
                let resolved = crate::probe::resolve_claude_exe(&cmd_path);
                let _ = CLAUDE_PATH.set(resolved);
                return;
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(std::path::Path::new("."));
        let candidate = dir.join("claude.exe");
        if candidate.exists() {
            let _ = CLAUDE_PATH.set(candidate.to_string_lossy().to_string());
        }
    }
}

pub fn get_claude_path() -> String {
    CLAUDE_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| "claude".to_string())
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected,
}

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

pub fn spawn(app: AppHandle, relay_url: String, token: String, mut stop: watch::Receiver<bool>) {
    tauri::async_runtime::spawn(async move {
        run_loop(app, relay_url, token, &mut stop).await;
    });
}

async fn run_loop(
    app: AppHandle,
    relay_url: String,
    token: String,
    stop: &mut watch::Receiver<bool>,
) {
    // Reconnect backoff: start small so a brief network blip recovers fast,
    // grow exponentially (with jitter) up to a cap so a relay outage doesn't
    // hammer the server or burn the laptop battery. Reset on a successful connect.
    const RECONNECT_BASE_SECS: u64 = 2;
    const RECONNECT_MAX_SECS: u64 = 60;
    let mut backoff_secs = RECONNECT_BASE_SECS;

    loop {
        if *stop.borrow() {
            break;
        }

        if relay_url.starts_with("http://") {
            wslog!("refusing plaintext ws:// connection; configure an https:// relay URL");
            let _ = app.emit("connection_state", ConnState::Disconnected);
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                _ = stop.changed() => { break; }
            }
            continue;
        }

        let _ = app.emit("connection_state", ConnState::Connecting);

        let ws_url = relay_url
            .replace("https://", "wss://")
            .trim_end_matches('/')
            .to_string()
            + "/agent";

        let mut req = match ws_url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => {
                wslog!("bad URL {ws_url}: {e}");
                let _ = app.emit("connection_state", ConnState::Disconnected);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                    _ = stop.changed() => { break; }
                }
                continue;
            }
        };
        let auth_value = match format!("Bearer {token}").parse() {
            Ok(v) => v,
            Err(e) => {
                wslog!("invalid auth token header ({e}); refusing to connect");
                let _ = app.emit("connection_state", ConnState::Disconnected);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                    _ = stop.changed() => { break; }
                }
                continue;
            }
        };
        req.headers_mut().insert("authorization", auth_value);

        let tls_connector = Connector::Rustls(crate::tls_pinning::build_tls_config());
        match connect_async_tls_with_config(req, None, false, Some(tls_connector)).await {
            Ok((ws_stream, _)) => {
                wslog!("connected to {ws_url}");
                let _ = app.emit("connection_state", ConnState::Connected);
                backoff_secs = RECONNECT_BASE_SECS; // healthy connection — reset backoff

                let (write, mut read) = ws_stream.split();
                let write = Arc::new(tokio::sync::Mutex::new(write));
                let relay_url_arc = Arc::new(relay_url.clone());
                let token_arc = Arc::new(token.clone());
                // Cap how many inbound messages we dispatch concurrently so a flood
                // of messages can't spawn unbounded handler tasks. Handlers are
                // short-lived (they verify the signature then spawn the actual work),
                // so this rarely engages; when it does the read loop briefly pauses,
                // applying natural backpressure on the socket.
                let dispatch_sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DISPATCH));

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            match msg {
                                Some(Ok(Message::Text(txt))) => {
                                    let permit = match dispatch_sem.clone().acquire_owned().await {
                                        Ok(p) => p,
                                        Err(_) => break, // semaphore closed — shouldn't happen
                                    };
                                    let write2 = write.clone();
                                    let txt_str = txt.to_string();
                                    let relay_url2 = relay_url_arc.clone();
                                    let token2 = token_arc.clone();
                                    let app2 = app.clone();
                                    tokio::spawn(async move {
                                        let _permit = permit; // released when the handler returns
                                        handle_text_message(&txt_str, write2, relay_url2, token2, app2).await;
                                    });
                                }
                                Some(Ok(Message::Ping(data))) => {
                                    let _ = write.lock().await.send(Message::Pong(data)).await;
                                }
                                Some(Ok(Message::Close(frame))) => {
                                    let code = frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(0);
                                    wslog!("connection closed by server: code={}", code);
                                    if code == 4001 {
                                        let _ = app.emit("force_unpair", ());
                                        return;
                                    }
                                    break;
                                }
                                None => { wslog!("stream ended (None)"); break; }
                                Some(Err(e)) => { wslog!("error: {e}"); break; }
                                _ => {}
                            }
                        }
                        _ = stop.changed() => {
                            if *stop.borrow() {
                                let _ = write.lock().await.send(Message::Close(None)).await;
                                return;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                wslog!("connect failed: {e}");
            }
        }

        let _ = app.emit("connection_state", ConnState::Disconnected);
        // Add up to ±25% jitter so many agents reconnecting after a relay restart
        // don't stampede in lockstep.
        let jitter_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_millis())
            .unwrap_or(0) as u64)
            % (backoff_secs * 500 + 1);
        let delay = std::time::Duration::from_secs(backoff_secs)
            + std::time::Duration::from_millis(jitter_ms);
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = stop.changed() => { break; }
        }
        backoff_secs = (backoff_secs * 2).min(RECONNECT_MAX_SECS);
    }
    let _ = app.emit("connection_state", ConnState::Disconnected);
}

// ── Message handling ──────────────────────────────────────────────────────────

async fn handle_text_message(
    txt: &str,
    write: Arc<tokio::sync::Mutex<WsSink>>,
    relay_url: Arc<String>,
    token: Arc<String>,
    app: AppHandle,
) {
    let msg: serde_json::Value = match serde_json::from_str(txt) {
        Ok(v) => v,
        Err(_) => return,
    };

    match msg.get("type").and_then(|t| t.as_str()) {
        // ── Relay welcome — no signature required (no capabilities, no side effects)
        Some("registered") => {
            capabilities::audit_log("registered", &[], true, None);
            handle_registered(msg, write, relay_url, token).await;
        }

        // ── Signed command envelope ──────────────────────────────────────────
        // All actionable commands from the relay MUST be wrapped in:
        //   { "type": "cmd", "cmd": "<payload JSON string>", "sig": "<HMAC-SHA256 hex>" }
        // The HMAC key is the agent bearer token. This ensures only the relay
        // (which holds the token) can produce valid signatures, providing
        // integrity against third-party injection.
        Some("cmd") => {
            let payload_str = msg["cmd"].as_str().unwrap_or("");
            let sig = msg["sig"].as_str().unwrap_or("");

            if payload_str.is_empty() || sig.is_empty() {
                capabilities::audit_log("cmd", &[], false, Some("missing_payload_or_sig"));
                wslog!("rejected cmd: missing payload or sig field");
                return;
            }

            if !capabilities::verify_command_sig(&token, payload_str, sig) {
                capabilities::audit_log("cmd", &[], false, Some("invalid_hmac_signature"));
                wslog!("SECURITY: rejected cmd — invalid HMAC signature (possible relay compromise or injection)");
                return;
            }

            let inner: serde_json::Value = match serde_json::from_str(payload_str) {
                Ok(v) => v,
                Err(_) => {
                    capabilities::audit_log("cmd", &[], false, Some("malformed_payload_json"));
                    wslog!("rejected cmd: malformed payload JSON after signature verification");
                    return;
                }
            };

            dispatch_command(inner, write, relay_url, token, app).await;
        }

        // ── Anything else is rejected (deny-by-default) ──────────────────────
        other => {
            let label = other.unwrap_or("(no type)");
            capabilities::audit_log(label, &[], false, Some("unsigned_or_unknown_envelope"));
            wslog!(
                "SECURITY: rejected message type '{}' — not 'registered' or signed 'cmd' envelope",
                label
            );
        }
    }
}

// ── dispatch_command: capability check + approval gate + action dispatch ─────

async fn dispatch_command(
    msg: serde_json::Value,
    write: Arc<tokio::sync::Mutex<WsSink>>,
    relay_url: Arc<String>,
    token: Arc<String>,
    app: AppHandle,
) {
    let msg_type = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");

    // Capability allowlist check (deny-by-default)
    let caps = match capabilities::message_capabilities(msg_type) {
        Some(c) => c,
        None => {
            capabilities::audit_log(msg_type, &[], false, Some("not_on_capability_allowlist"));
            wslog!(
                "SECURITY: rejected '{}': not on capability allowlist",
                msg_type
            );
            return;
        }
    };

    capabilities::audit_log(msg_type, caps, true, None);

    // User approval gate for dangerous operations
    if capabilities::requires_approval(msg_type) {
        let description = format_approval_description(&msg);
        let action_id = msg
            .get("pushId")
            .or(msg.get("taskId"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let approved = request_approval(&app, msg_type, &description, &action_id).await;
        if !approved {
            capabilities::audit_log(msg_type, caps, false, Some("denied_by_user_or_timeout"));
            wslog!(
                "action '{}' denied (user declined or 30s timeout)",
                msg_type
            );
            send_denial_response(msg_type, &msg, &write).await;
            return;
        }
        capabilities::audit_log(msg_type, caps, true, Some("approved_by_user"));
    }

    // Dispatch to the actual handler
    dispatch_action(msg, write, relay_url, token, app).await;
}

fn format_approval_description(msg: &serde_json::Value) -> String {
    match msg.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "git_push" => {
            let path = msg["projectPath"].as_str().unwrap_or("unknown path");
            let commit = msg["commitMessage"]
                .as_str()
                .unwrap_or("Outpost: code changes");
            format!("Push to: {}\nCommit message: {}", path, commit)
        }
        other => format!("Action: {}", other),
    }
}

async fn send_denial_response(
    msg_type: &str,
    msg: &serde_json::Value,
    write: &Arc<tokio::sync::Mutex<WsSink>>,
) {
    let response = match msg_type {
        "git_push" => {
            let push_id = msg["pushId"].as_str().unwrap_or("");
            serde_json::json!({
                "type": "git_push_error",
                "pushId": push_id,
                "error": "Permission denied by user"
            })
        }
        _ => return,
    };
    let _ = write
        .lock()
        .await
        .send(Message::Text(response.to_string().into()))
        .await;
}

/// Request user approval for a dangerous action. Emits a `permission_request`
/// Tauri event and waits up to 30 seconds for the user to respond. Fails closed
/// (deny) on timeout — the agent window may be unattended but security holds.
async fn request_approval(
    app: &AppHandle,
    action_type: &str,
    description: &str,
    action_id: &str,
) -> bool {
    let (tx, rx) = oneshot::channel::<bool>();
    let request_id = format!("{}-{}", action_type, action_id);

    {
        let state = app.state::<std::sync::Mutex<crate::ApprovalState>>();
        let mut guard = state.lock().unwrap();
        guard.pending.insert(request_id.clone(), tx);
    }

    let _ = app.emit(
        "permission_request",
        serde_json::json!({
            "requestId": request_id,
            "actionType": action_type,
            "description": description,
        }),
    );

    match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
        Ok(Ok(approved)) => {
            let _ = app.emit(
                "permission_resolved",
                serde_json::json!({
                    "requestId": request_id,
                    "approved": approved,
                }),
            );
            approved
        }
        _ => {
            // Timeout or channel dropped — remove stale entry and deny
            if let Ok(mut guard) = app.state::<std::sync::Mutex<crate::ApprovalState>>().lock() {
                guard.pending.remove(&request_id);
            }
            let _ = app.emit(
                "permission_resolved",
                serde_json::json!({
                    "requestId": request_id,
                    "approved": false,
                    "reason": "timeout",
                }),
            );
            wslog!(
                "approval for '{}' timed out after 30s — denied",
                action_type
            );
            false
        }
    }
}

// ── handle_registered: on every connection ────────────────────────────────────

async fn handle_registered(
    _msg: serde_json::Value,
    write: Arc<tokio::sync::Mutex<WsSink>>,
    relay_url: Arc<String>,
    token: Arc<String>,
) {
    let write2 = write.clone();
    tokio::spawn(async move {
        let begin = serde_json::json!({ "type": "startup_begin" });
        let _ = write2
            .lock()
            .await
            .send(Message::Text(begin.to_string().into()))
            .await;

        let hostname = crate::config::get_hostname();
        let result = tokio::task::spawn_blocking(move || crate::probe::run_probe(&hostname))
            .await
            .unwrap_or_else(|_| crate::probe::run_probe("unknown"));

        if result.claude_installed && !result.claude_path.is_empty() {
            let _ = CLAUDE_PATH.set(result.claude_path.clone());
        }

        let count = result.projects.len();
        for project in &result.projects {
            crate::task_runner::ensure_project_setup(&project.path);
        }

        let complete = serde_json::json!({
            "type": "startup_complete",
            "projectCount": count,
        });
        let _ = write2
            .lock()
            .await
            .send(Message::Text(complete.to_string().into()))
            .await;

        let relay3 = relay_url.clone();
        let tok3 = token.clone();
        let sync_paths: Vec<String> = result.projects.iter().map(|p| p.path.clone()).collect();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let cutoff = std::time::SystemTime::now()
                .checked_sub(std::time::Duration::from_secs(30 * 24 * 3600))
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            for proj_path in &sync_paths {
                // Startup backfill scans outpost-output/ only. Snapshotting the
                // current tree means the new-deliverable fallback is a no-op here
                // (nothing is being created during sync).
                let snap = crate::artifact_uploader::snapshot_project_files(proj_path);
                let _ = crate::artifact_uploader::upload_new_artifacts(
                    proj_path, "", &relay3, &tok3, cutoff, true, &snap,
                )
                .await;
            }
        });
    });
}

// ── dispatch_action: all verified, capability-checked commands ────────────────

async fn dispatch_action(
    msg: serde_json::Value,
    write: Arc<tokio::sync::Mutex<WsSink>>,
    relay_url: Arc<String>,
    token: Arc<String>,
    _app: AppHandle,
) {
    match msg.get("type").and_then(|t| t.as_str()) {
        Some("setup_project") => {
            let path = msg["path"].as_str().unwrap_or("").to_string();
            if !path.is_empty() && crate::task_runner::is_path_within_home(&path) {
                tokio::task::spawn_blocking(move || {
                    crate::task_runner::ensure_project_setup(&path);
                });
            } else if !path.is_empty() {
                wslog!(
                    "setup_project: rejected path outside home directory: {}",
                    path
                );
            }
        }

        Some("probe") => {
            let probe_id = msg
                .get("probeId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let hostname = crate::config::get_hostname();
            let result = tokio::task::spawn_blocking(move || probe::run_probe(&hostname))
                .await
                .unwrap_or_else(|_| probe::run_probe("unknown"));

            if result.claude_installed && !result.claude_path.is_empty() {
                let _ = CLAUDE_PATH.set(result.claude_path.clone());
            }

            let response = serde_json::json!({
                "type": "probe_result",
                "probeId": probe_id,
                "data": result,
            });
            let _ = write
                .lock()
                .await
                .send(Message::Text(response.to_string().into()))
                .await;
        }

        Some("create_project") => {
            let create_id = msg["createId"].as_str().unwrap_or("").to_string();
            let name = msg["name"].as_str().unwrap_or("").to_string();
            let parent_dir = msg["parentDir"].as_str().map(|s| s.to_string());

            if create_id.is_empty() || name.is_empty() {
                return;
            }

            let write2 = write.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    crate::task_runner::create_project(&name, parent_dir.as_deref())
                })
                .await
                .unwrap_or_else(|_| Err("Task panicked".to_string()));

                let response = match result {
                    Ok(path) => serde_json::json!({
                        "type": "project_created",
                        "createId": create_id,
                        "path": path,
                    }),
                    Err(e) => serde_json::json!({
                        "type": "project_create_error",
                        "createId": create_id,
                        "error": e,
                    }),
                };
                let _ = write2
                    .lock()
                    .await
                    .send(Message::Text(response.to_string().into()))
                    .await;
            });
        }

        Some("list_project_files") => {
            let request_id = msg["requestId"].as_str().unwrap_or("").to_string();
            let project_path = msg["projectPath"].as_str().unwrap_or("").to_string();
            if request_id.is_empty() || project_path.is_empty() {
                return;
            }

            let write2 = write.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    crate::task_runner::list_project_files(&project_path)
                })
                .await
                .unwrap_or_else(|_| Err("File scan panicked".to_string()));

                let response = match result {
                    Ok(files) => serde_json::json!({
                        "type": "project_files_result",
                        "requestId": request_id,
                        "files": files,
                    }),
                    Err(error) => serde_json::json!({
                        "type": "project_files_error",
                        "requestId": request_id,
                        "error": error,
                    }),
                };
                let _ = write2
                    .lock()
                    .await
                    .send(Message::Text(response.to_string().into()))
                    .await;
            });
        }

        Some("list_directories") => {
            let request_id = msg["requestId"].as_str().unwrap_or("").to_string();
            let path = msg["path"].as_str().map(|s| s.to_string());
            if request_id.is_empty() {
                return;
            }

            let write2 = write.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    crate::task_runner::list_directories(path.as_deref())
                })
                .await
                .unwrap_or_else(|_| Err("Directory browse panicked".to_string()));

                let response = match result {
                    Ok(result) => serde_json::json!({
                        "type": "directories_result",
                        "requestId": request_id,
                        "result": result,
                    }),
                    Err(error) => serde_json::json!({
                        "type": "directories_error",
                        "requestId": request_id,
                        "error": error,
                    }),
                };
                let _ = write2
                    .lock()
                    .await
                    .send(Message::Text(response.to_string().into()))
                    .await;
            });
        }

        Some("probe_runtimes") => {
            let request_id = msg["requestId"].as_str().unwrap_or("").to_string();
            if request_id.is_empty() {
                return;
            }
            let write2 = write.clone();
            tokio::spawn(async move {
                let runtimes = tokio::task::spawn_blocking(probe::probe_runtimes)
                    .await
                    .unwrap_or_default();
                let response = serde_json::json!({
                    "type": "runtimes_result",
                    "requestId": request_id,
                    "runtimes": runtimes,
                });
                let _ = write2
                    .lock()
                    .await
                    .send(Message::Text(response.to_string().into()))
                    .await;
            });
        }

        Some("start_run") => {
            let run_id = msg["runId"].as_str().unwrap_or("").to_string();
            let project_path = msg["projectPath"].as_str().unwrap_or("").to_string();
            let prompt = msg["prompt"].as_str().unwrap_or("").to_string();
            // Runtime command + arg template come from the relay's runtimeConfig.
            let command = msg
                .get("runtimeConfig")
                .and_then(|c| c.get("command"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args_template: Vec<String> = msg
                .get("runtimeConfig")
                .and_then(|c| c.get("argsTemplate"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| a.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            if run_id.is_empty() || project_path.is_empty() || prompt.is_empty() {
                return;
            }

            let write2 = write.clone();
            tokio::spawn(async move {
                let (program, args) =
                    match run_executor::build_invocation(&command, &args_template, &prompt) {
                        Ok(v) => v,
                        Err(e) => {
                            let response = serde_json::json!({
                                "type": "run_status",
                                "runId": run_id,
                                "status": "failed",
                                "error": e,
                            });
                            let _ = write2
                                .lock()
                                .await
                                .send(Message::Text(response.to_string().into()))
                                .await;
                            return;
                        }
                    };

                // Tell the relay we're starting.
                let starting = serde_json::json!({
                    "type": "run_status",
                    "runId": run_id,
                    "status": "running",
                });
                let _ = write2
                    .lock()
                    .await
                    .send(Message::Text(starting.to_string().into()))
                    .await;

                let mut rx = run_executor::spawn_run(
                    run_id.clone(),
                    project_path,
                    program,
                    args,
                    std::collections::HashMap::new(),
                );

                while let Some(event) = rx.recv().await {
                    let payload = match event {
                        run_executor::RunEvent::Output { line, stream } => serde_json::json!({
                            "type": "run_event",
                            "runId": run_id,
                            "event": "output",
                            "line": line,
                            "stream": stream,
                        }),
                        run_executor::RunEvent::Step { text } => serde_json::json!({
                            "type": "run_event",
                            "runId": run_id,
                            "event": "step",
                            "text": text,
                        }),
                        run_executor::RunEvent::Summary { text } => serde_json::json!({
                            "type": "run_event",
                            "runId": run_id,
                            "event": "summary",
                            "text": text,
                        }),
                        run_executor::RunEvent::Diff {
                            files,
                            additions,
                            deletions,
                            patch,
                        } => serde_json::json!({
                            "type": "run_event",
                            "runId": run_id,
                            "event": "diff",
                            "files": files,
                            "additions": additions,
                            "deletions": deletions,
                            "patch": patch,
                        }),
                        run_executor::RunEvent::Done { exit_code, status } => serde_json::json!({
                            "type": "run_status",
                            "runId": run_id,
                            "status": status,
                            "exitCode": exit_code,
                        }),
                        run_executor::RunEvent::Failed { error } => serde_json::json!({
                            "type": "run_status",
                            "runId": run_id,
                            "status": "failed",
                            "error": error,
                        }),
                    };
                    if write2
                        .lock()
                        .await
                        .send(Message::Text(payload.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }

        Some("cancel_run") => {
            let run_id = msg["runId"].as_str().unwrap_or("").to_string();
            if !run_id.is_empty() {
                run_executor::cancel_run(&run_id);
            }
        }

        Some("secret_grant") => {
            // A user-approved secret for a run. Store it (value never logged) so
            // the run can use it. The relay only sends this after phone approval.
            let run_id = msg["runId"].as_str().unwrap_or("").to_string();
            let secret_name = msg["secretName"].as_str().unwrap_or("").to_string();
            let value = msg["value"].as_str().unwrap_or("").to_string();
            if !run_id.is_empty() && !secret_name.is_empty() && !value.is_empty() {
                run_executor::store_secret_grant(&run_id, &secret_name, &value);
                wslog!("secret_grant stored for run {} ({})", run_id, secret_name);
            }
        }

        Some("setup_ollama") => {
            let request_id = msg["requestId"].as_str().unwrap_or("").to_string();
            let model_id = msg["modelId"].as_str().unwrap_or("").to_string();
            let endpoint = msg["endpoint"]
                .as_str()
                .unwrap_or("http://localhost:11434")
                .to_string();
            if request_id.is_empty() || model_id.is_empty() {
                return;
            }
            if let Err(e) = crate::task_runner::validate_ollama_endpoint(&endpoint) {
                wslog!("setup_ollama: rejected endpoint: {}", e);
                let write2 = write.clone();
                let _ = write2
                    .lock()
                    .await
                    .send(Message::Text(
                        serde_json::json!({
                            "type": "ollama_setup_error",
                            "requestId": request_id,
                            "error": e,
                        })
                        .to_string()
                        .into(),
                    ))
                    .await;
                return;
            }

            let write2 = write.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    crate::task_runner::setup_ollama(&model_id, &endpoint)
                })
                .await
                .unwrap_or_else(|_| Err("Ollama setup panicked".to_string()));

                let response = match result {
                    Ok(data) => serde_json::json!({
                        "type": "ollama_setup_result",
                        "requestId": request_id,
                        "data": data,
                    }),
                    Err(error) => serde_json::json!({
                        "type": "ollama_setup_error",
                        "requestId": request_id,
                        "error": error,
                    }),
                };
                let _ = write2
                    .lock()
                    .await
                    .send(Message::Text(response.to_string().into()))
                    .await;
            });
        }

        Some("clone_repo") => {
            let clone_id = msg["cloneId"].as_str().unwrap_or("").to_string();
            let repo_url = msg["repoUrl"].as_str().unwrap_or("").to_string();
            let name = msg["name"].as_str().unwrap_or("").to_string();
            if clone_id.is_empty() || repo_url.is_empty() || name.is_empty() {
                return;
            }

            let write2 = write.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    crate::task_runner::clone_repo(&repo_url, &name)
                })
                .await
                .unwrap_or_else(|_| Err("Clone panicked".to_string()));

                let response = match result {
                    Ok(path) => serde_json::json!({
                        "type": "clone_result",
                        "cloneId": clone_id,
                        "path": path,
                    }),
                    Err(e) => serde_json::json!({
                        "type": "clone_error",
                        "cloneId": clone_id,
                        "error": e,
                    }),
                };
                let _ = write2
                    .lock()
                    .await
                    .send(Message::Text(response.to_string().into()))
                    .await;
            });
        }

        Some("git_push") => {
            let push_id = msg["pushId"].as_str().unwrap_or("").to_string();
            let project_path = msg["projectPath"].as_str().unwrap_or("").to_string();
            let commit_message = msg["commitMessage"]
                .as_str()
                .unwrap_or("Outpost: code changes")
                .to_string();
            let github_token = msg["githubToken"].as_str().map(|s| s.to_string());
            if push_id.is_empty() || project_path.is_empty() {
                return;
            }

            let write2 = write.clone();
            tokio::spawn(async move {
                let proj = project_path.clone();
                let msg_clone = commit_message.clone();
                let tok_clone = github_token.clone();
                let result = tokio::task::spawn_blocking(move || {
                    crate::task_runner::git_push(&proj, &msg_clone, tok_clone.as_deref())
                })
                .await
                .unwrap_or_else(|_| Err("Git push panicked".to_string()));

                let response = match result {
                    Ok(output) => {
                        // After a successful push, try to open/find a GitHub PR.
                        let pr_url = if let Some(token) = &github_token {
                            create_or_find_github_pr(&project_path, &commit_message, token).await
                        } else {
                            None
                        };
                        let mut v = serde_json::json!({
                            "type": "git_push_result",
                            "pushId": push_id,
                            "output": output,
                        });
                        if let Some(url) = pr_url {
                            v["prUrl"] = serde_json::Value::String(url);
                        }
                        v
                    }
                    Err(e) => serde_json::json!({
                        "type": "git_push_error",
                        "pushId": push_id,
                        "error": e,
                    }),
                };
                let _ = write2
                    .lock()
                    .await
                    .send(Message::Text(response.to_string().into()))
                    .await;
            });
        }

        Some("run_task") => {
            let task_id = msg["taskId"].as_str().unwrap_or("").to_string();
            let project_path = msg["projectPath"].as_str().unwrap_or("").to_string();
            let prompt = msg["prompt"].as_str().unwrap_or("").to_string();
            let session_memory = msg["sessionMemory"].as_str().map(|s| s.to_string());
            let provider_id = msg["providerId"]
                .as_str()
                .unwrap_or("anthropic")
                .to_string();
            let model_id = msg["modelId"].as_str().map(|s| s.to_string());
            let endpoint = msg["endpoint"].as_str().map(|s| s.to_string());
            let api_key = msg["apiKey"].as_str().map(|s| s.to_string());
            let use_wsl = msg["useWsl"].as_bool().unwrap_or(false);
            let wsl_distro = msg["wslDistro"].as_str().map(|s| s.to_string());
            let is_code_task = msg["isCodeTask"].as_bool().unwrap_or(false);
            // Vault secrets injected by relay — set as env vars so tasks can call
            // tools that read VERCEL_TOKEN, GITHUB_TOKEN, etc. from the environment.
            let vault_secrets: std::collections::HashMap<String, String> = msg["vaultSecrets"]
                .as_object()
                .map(|map| {
                    map.iter()
                        .filter_map(|(k, v)| {
                            // Guard: only valid env-var identifiers pass through.
                            let safe = k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                                && !k.is_empty()
                                && k.len() <= 128;
                            if safe {
                                v.as_str().map(|s| (k.clone(), s.to_string()))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();

            if task_id.is_empty() || project_path.is_empty() || prompt.is_empty() {
                return;
            }

            let write2 = write.clone();
            let tid = task_id.clone();
            let proj = project_path.clone();
            let claude_path = get_claude_path();
            let relay_url2 = relay_url.clone();
            let token2 = token.clone();

            tokio::spawn(async move {
                let task_started_at = std::time::SystemTime::now();
                // Capture pre-existing files so we can later distinguish newly
                // created deliverables from edits to files that already existed.
                let pre_existing = crate::artifact_uploader::snapshot_project_files(&proj);
                let engine = task_runner::TaskEngine {
                    provider_id,
                    model_id,
                    endpoint,
                    api_key,
                    is_code_task,
                };
                let mut rx = task_runner::spawn_task(
                    tid.clone(),
                    proj.clone(),
                    prompt,
                    session_memory,
                    engine,
                    use_wsl,
                    wsl_distro,
                    claude_path,
                    vault_secrets,
                );
                while let Some(event) = rx.recv().await {
                    let is_terminal = matches!(
                        event,
                        task_runner::TaskEvent::Done { .. } | task_runner::TaskEvent::Error(_)
                    );
                    let msg = match event {
                        task_runner::TaskEvent::Output { data, stream } => serde_json::json!({
                            "type": "output",
                            "taskId": tid,
                            "data": data,
                            "stream": stream,
                        }),
                        task_runner::TaskEvent::Done { exit_code } => serde_json::json!({
                            "type": "done",
                            "taskId": tid,
                            "exitCode": exit_code,
                        }),
                        task_runner::TaskEvent::Error(e) => serde_json::json!({
                            "type": "done",
                            "taskId": tid,
                            "exitCode": -1,
                            "error": e,
                        }),
                    };
                    if write2
                        .lock()
                        .await
                        .send(Message::Text(msg.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if is_terminal {
                        let write3 = write2.clone();
                        let tid3 = tid.clone();
                        let proj3 = proj.clone();
                        let relay3 = relay_url2.clone();
                        let tok3 = token2.clone();
                        let pre_existing3 = pre_existing.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                            // Collect git diff and forward to the phone
                            let changes = tokio::task::spawn_blocking({
                                let p = proj3.clone();
                                move || crate::task_runner::collect_git_changes(&p)
                            })
                            .await
                            .unwrap_or_else(|_| {
                                crate::task_runner::GitChanges {
                                    files: vec![],
                                    is_git_repo: false,
                                }
                            });
                            if changes.is_git_repo && !changes.files.is_empty() {
                                let diff_msg = serde_json::json!({
                                    "type": "files_changed",
                                    "taskId": tid3,
                                    "files": changes.files,
                                    "isGitRepo": changes.is_git_repo,
                                });
                                let _ = write3
                                    .lock()
                                    .await
                                    .send(Message::Text(diff_msg.to_string().into()))
                                    .await;
                            }
                            let artifacts = crate::artifact_uploader::upload_new_artifacts(
                                &proj3,
                                &tid3,
                                &relay3,
                                &tok3,
                                task_started_at,
                                is_code_task,
                                &pre_existing3,
                            )
                            .await;
                            if !artifacts.is_empty() {
                                let filenames: Vec<String> =
                                    artifacts.iter().map(|a| a.filename.clone()).collect();
                                let notify = serde_json::json!({
                                    "type": "artifacts_ready",
                                    "taskId": tid3,
                                    "projectPath": proj3,
                                    "count": artifacts.len(),
                                    "filenames": filenames,
                                });
                                let _ = write3
                                    .lock()
                                    .await
                                    .send(Message::Text(notify.to_string().into()))
                                    .await;
                            }
                        });
                        break;
                    }
                }
            });
        }

        Some("cancel_task") => {
            let task_id = msg["taskId"].as_str().unwrap_or("").to_string();
            if !task_id.is_empty() {
                let cancelled = crate::task_runner::cancel_task(&task_id);
                wslog!("cancel_task {} -> {}", task_id, cancelled);
            }
        }

        _ => {}
    }
}

// ── GitHub PR creation ────────────────────────────────────────────────────────

/// After a successful push, try to find an existing open PR for the current
/// branch or create a new one. Returns the HTML URL on success, None on any
/// error (network, auth, already up-to-date branch, etc.) — the push itself
/// is not affected.
async fn create_or_find_github_pr(
    project_path: &str,
    commit_message: &str,
    token: &str,
) -> Option<String> {
    // Read the remote URL and current branch (blocking git calls).
    let (remote_url, current_branch) = tokio::task::spawn_blocking({
        let p = project_path.to_string();
        move || {
            let remote = crate::task_runner::run_git_readonly(&["remote", "get-url", "origin"], &p)
                .unwrap_or_default();
            let branch =
                crate::task_runner::run_git_readonly(&["symbolic-ref", "--short", "HEAD"], &p)
                    .unwrap_or_default();
            (remote.trim().to_string(), branch.trim().to_string())
        }
    })
    .await
    .ok()?;

    // Only works for GitHub HTTPS remotes.
    if !remote_url.starts_with("https://") || !remote_url.contains("github.com") {
        return None;
    }
    if current_branch.is_empty() {
        return None;
    }

    let (owner, repo) = parse_github_owner_repo(&remote_url)?;

    // GitHub API calls must NOT use the relay-pinned client — that client pins
    // the relay's SPKI and would reject api.github.com's certificate. Use a
    // standard client with native roots instead.
    let client = github_api_client();
    let api_base = format!("https://api.github.com/repos/{owner}/{repo}");
    let auth_header = format!("Bearer {}", token);
    let send = |req: reqwest::RequestBuilder| {
        req.header("Authorization", &auth_header)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "outpost-agent/0.1")
    };

    // Resolve the repository's real default branch (don't guess "main").
    let repo_resp = send(client.get(&api_base)).send().await.ok()?;
    if !repo_resp.status().is_success() {
        return None;
    }
    let repo_info: serde_json::Value = repo_resp.json().await.ok()?;
    let default_branch = repo_info["default_branch"]
        .as_str()
        .unwrap_or("main")
        .to_string();

    // Don't open a PR when pushing straight to the default branch.
    if current_branch == default_branch {
        return None;
    }

    // Check for an existing open PR for this head branch. Query params are passed
    // via .query() so branch names with '/', '#', etc. are percent-encoded.
    let list_resp = send(client.get(format!("{api_base}/pulls")))
        .query(&[
            ("state", "open"),
            ("head", &format!("{owner}:{current_branch}")),
            ("per_page", "1"),
        ])
        .send()
        .await
        .ok()?;

    if list_resp.status().is_success() {
        let items: serde_json::Value = list_resp.json().await.ok()?;
        if let Some(url) = items[0]["html_url"].as_str() {
            return Some(url.to_string()); // PR already exists
        }
    }

    // Derive a PR title from the commit message (first line, max 72 chars).
    let title: String = commit_message
        .lines()
        .next()
        .unwrap_or("Outpost changes")
        .chars()
        .take(72)
        .collect();

    let body = serde_json::json!({
        "title": title,
        "head": current_branch,
        "base": default_branch,
        "body": "Created automatically by Outpost after a commit & push.",
    });

    let create_resp = send(client.post(format!("{api_base}/pulls")))
        .json(&body)
        .send()
        .await
        .ok()?;

    if create_resp.status().is_success() {
        let pr: serde_json::Value = create_resp.json().await.ok()?;
        pr["html_url"].as_str().map(|s| s.to_string())
    } else {
        None
    }
}

/// A standard (non-pinned) HTTPS client for GitHub API calls. The pinned client
/// is reserved for relay-bound traffic; GitHub presents its own certificate
/// chain validated against the system/native roots.
fn github_api_client() -> &'static reqwest::Client {
    static GITHUB_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    GITHUB_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Parse `https://github.com/owner/repo.git` → `("owner", "repo")`.
fn parse_github_owner_repo(url: &str) -> Option<(String, String)> {
    let path = url
        .trim_end_matches(".git")
        .trim_end_matches('/')
        .split("github.com/")
        .nth(1)?;
    let mut parts = path.splitn(2, '/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}
