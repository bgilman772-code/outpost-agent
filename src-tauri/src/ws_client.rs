use futures_util::{SinkExt, StreamExt};
use tauri::{AppHandle, Emitter};
use tokio::sync::watch;
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
use tokio_tungstenite::tungstenite::Message;
use std::sync::OnceLock;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

macro_rules! wslog {
    ($($arg:tt)*) => {
        eprintln!("[ws] {}", format!($($arg)*));
    };
}

use crate::probe;
use crate::task_runner;

static CLAUDE_PATH: OnceLock<String> = OnceLock::new();

pub fn init_claude_path() {
    if CLAUDE_PATH.get().is_some() { return; }
    // Resolve via `where claude` so we get the full .exe path, not a .cmd wrapper
    #[allow(unused_mut)]
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/C", "where", "claude"]);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    if let Ok(out) = cmd.output() {
        if out.status.success() {
            let cmd_path = String::from_utf8_lossy(&out.stdout)
                .lines().next().unwrap_or("").trim().to_string();
            if !cmd_path.is_empty() {
                let resolved = crate::probe::resolve_claude_exe(&cmd_path);
                let _ = CLAUDE_PATH.set(resolved);
                return;
            }
        }
    }
    // Fallback: look next to the agent binary
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(std::path::Path::new("."));
        let candidate = dir.join("claude.exe");
        if candidate.exists() {
            let _ = CLAUDE_PATH.set(candidate.to_string_lossy().to_string());
        }
    }
}

pub fn get_claude_path() -> String {
    CLAUDE_PATH.get().cloned().unwrap_or_else(|| "claude".to_string())
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected,
}

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
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
    loop {
        if *stop.borrow() {
            break;
        }

        // Refuse plaintext connections — only wss:// (https://) is permitted
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
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {token}").parse().unwrap(),
        );

        match connect_async(req).await {
            Ok((ws_stream, _)) => {
                wslog!("connected to {ws_url}");
                let _ = app.emit("connection_state", ConnState::Connected);

                let (write, mut read) = ws_stream.split();
                let write = std::sync::Arc::new(tokio::sync::Mutex::new(write));
                let relay_url_arc = std::sync::Arc::new(relay_url.clone());
                let token_arc = std::sync::Arc::new(token.clone());

                loop {
                    tokio::select! {
                        msg = read.next() => {
                            match msg {
                                Some(Ok(Message::Text(txt))) => {
                                    let write2 = write.clone();
                                    let txt_str = txt.to_string();
                                    let relay_url2 = relay_url_arc.clone();
                                    let token2 = token_arc.clone();
                                    tokio::spawn(async move {
                                        handle_text_message(&txt_str, write2, relay_url2, token2).await;
                                    });
                                }
                                Some(Ok(Message::Ping(data))) => {
                                    let _ = write.lock().await.send(Message::Pong(data)).await;
                                }
                                Some(Ok(Message::Close(frame))) => {
                                    let code = frame.as_ref().map(|f| u16::from(f.code)).unwrap_or(0);
                                    wslog!("connection closed by server: code={}", code);
                                    if code == 4001 {
                                        // Machine was deleted from the relay — must re-pair
                                        let _ = app.emit("force_unpair", ());
                                        return;
                                    }
                                    break;
                                }
                                None => {
                                    wslog!("stream ended (None)");
                                    break;
                                }
                                Some(Err(e)) => {
                                    wslog!("error: {e}");
                                    break;
                                }
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
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            _ = stop.changed() => { break; }
        }
    }
    let _ = app.emit("connection_state", ConnState::Disconnected);
}

async fn handle_text_message(
    txt: &str,
    write: std::sync::Arc<tokio::sync::Mutex<WsSink>>,
    relay_url: std::sync::Arc<String>,
    token: std::sync::Arc<String>,
) {
    let msg: serde_json::Value = match serde_json::from_str(txt) {
        Ok(v) => v,
        Err(_) => return,
    };

    match msg.get("type").and_then(|t| t.as_str()) {
        Some("registered") => {
            // On every connection: scan projects, create CLAUDE.md + outpost-output/ for each
            let write2 = write.clone();
            tokio::spawn(async move {
                let begin = serde_json::json!({ "type": "startup_begin" });
                let _ = write2.lock().await.send(Message::Text(begin.to_string().into())).await;

                let hostname = crate::config::get_hostname();
                let result = tokio::task::spawn_blocking(move || crate::probe::run_probe(&hostname))
                    .await
                    .unwrap_or_else(|_| crate::probe::run_probe("unknown"));

                // Cache claude path while we have it
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
                let _ = write2.lock().await.send(Message::Text(complete.to_string().into())).await;

                // Background: sync existing outpost-output/ files to the relay
                let relay3 = relay_url.clone();
                let tok3 = token.clone();
                let sync_paths: Vec<String> = result.projects.iter().map(|p| p.path.clone()).collect();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let cutoff = std::time::SystemTime::now()
                        .checked_sub(std::time::Duration::from_secs(30 * 24 * 3600))
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    for proj_path in &sync_paths {
                        let _ = crate::artifact_uploader::upload_new_artifacts(
                            proj_path, "", &relay3, &tok3, cutoff, true,
                        ).await;
                    }
                });
            });
        }

        Some("setup_project") => {
            let path = msg["path"].as_str().unwrap_or("").to_string();
            if !path.is_empty() && crate::task_runner::is_path_within_home(&path) {
                tokio::task::spawn_blocking(move || {
                    crate::task_runner::ensure_project_setup(&path);
                });
            } else if !path.is_empty() {
                wslog!("setup_project: rejected path outside home directory: {}", path);
            }
        }

        Some("probe") => {
            let probe_id = msg.get("probeId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let hostname = crate::config::get_hostname();
            let result = tokio::task::spawn_blocking(move || probe::run_probe(&hostname))
                .await
                .unwrap_or_else(|_| probe::run_probe("unknown"));

            // Cache the resolved claude path for task execution
            if result.claude_installed && !result.claude_path.is_empty() {
                let _ = CLAUDE_PATH.set(result.claude_path.clone());
            }

            let response = serde_json::json!({
                "type": "probe_result",
                "probeId": probe_id,
                "data": result,
            });
            let _ = write.lock().await.send(Message::Text(response.to_string().into())).await;
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
                }).await.unwrap_or_else(|_| Err("Task panicked".to_string()));

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
                let _ = write2.lock().await.send(Message::Text(response.to_string().into())).await;
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
                }).await.unwrap_or_else(|_| Err("File scan panicked".to_string()));

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
                let _ = write2.lock().await.send(Message::Text(response.to_string().into())).await;
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
                }).await.unwrap_or_else(|_| Err("Directory browse panicked".to_string()));

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
                let _ = write2.lock().await.send(Message::Text(response.to_string().into())).await;
            });
        }

        Some("setup_ollama") => {
            let request_id = msg["requestId"].as_str().unwrap_or("").to_string();
            let model_id = msg["modelId"].as_str().unwrap_or("").to_string();
            let endpoint = msg["endpoint"].as_str().unwrap_or("http://localhost:11434").to_string();
            if request_id.is_empty() || model_id.is_empty() {
                return;
            }
            if let Err(e) = crate::task_runner::validate_ollama_endpoint(&endpoint) {
                wslog!("setup_ollama: rejected endpoint: {}", e);
                let write2 = write.clone();
                let _ = write2.lock().await.send(Message::Text(serde_json::json!({
                    "type": "ollama_setup_error",
                    "requestId": request_id,
                    "error": e,
                }).to_string().into())).await;
                return;
            }

            let write2 = write.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    crate::task_runner::setup_ollama(&model_id, &endpoint)
                }).await.unwrap_or_else(|_| Err("Ollama setup panicked".to_string()));

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
                let _ = write2.lock().await.send(Message::Text(response.to_string().into())).await;
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
                }).await.unwrap_or_else(|_| Err("Clone panicked".to_string()));

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
                let _ = write2.lock().await.send(Message::Text(response.to_string().into())).await;
            });
        }

        Some("git_push") => {
            let push_id = msg["pushId"].as_str().unwrap_or("").to_string();
            let project_path = msg["projectPath"].as_str().unwrap_or("").to_string();
            let commit_message = msg["commitMessage"].as_str().unwrap_or("Outpost: code changes").to_string();
            let github_token = msg["githubToken"].as_str().map(|s| s.to_string());

            if push_id.is_empty() || project_path.is_empty() {
                return;
            }

            let write2 = write.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    crate::task_runner::git_push(&project_path, &commit_message, github_token.as_deref())
                }).await.unwrap_or_else(|_| Err("Git push panicked".to_string()));

                let response = match result {
                    Ok(output) => serde_json::json!({
                        "type": "git_push_result",
                        "pushId": push_id,
                        "output": output,
                    }),
                    Err(e) => serde_json::json!({
                        "type": "git_push_error",
                        "pushId": push_id,
                        "error": e,
                    }),
                };
                let _ = write2.lock().await.send(Message::Text(response.to_string().into())).await;
            });
        }

        Some("run_task") => {
            let task_id = msg["taskId"].as_str().unwrap_or("").to_string();
            let project_path = msg["projectPath"].as_str().unwrap_or("").to_string();
            let prompt = msg["prompt"].as_str().unwrap_or("").to_string();
            let session_memory = msg["sessionMemory"].as_str().map(|s| s.to_string());
            let provider_id = msg["providerId"].as_str().unwrap_or("anthropic").to_string();
            let model_id = msg["modelId"].as_str().map(|s| s.to_string());
            let endpoint = msg["endpoint"].as_str().map(|s| s.to_string());
            let api_key = msg["apiKey"].as_str().map(|s| s.to_string());
            let use_wsl = msg["useWsl"].as_bool().unwrap_or(false);
            let wsl_distro = msg["wslDistro"].as_str().map(|s| s.to_string());
            let is_code_task = msg["isCodeTask"].as_bool().unwrap_or(false);

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
                // Snapshot time just before Claude starts so we catch all output files
                let task_started_at = std::time::SystemTime::now();

                let engine = task_runner::TaskEngine { provider_id, model_id, endpoint, api_key, is_code_task };
                let mut rx = task_runner::spawn_task(tid.clone(), proj.clone(), prompt, session_memory, engine, use_wsl, wsl_distro, claude_path);
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
                    if write2.lock().await.send(Message::Text(msg.to_string().into())).await.is_err() {
                        break;
                    }
                    if is_terminal {
                        // Upload any files created/modified during the task, then notify phone
                        let write3 = write2.clone();
                        let tid3 = tid.clone();
                        let proj3 = proj.clone();
                        let relay3 = relay_url2.clone();
                        let tok3 = token2.clone();
                        tokio::spawn(async move {
                            // Small delay to let file system flush any final writes
                            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                            let artifacts = crate::artifact_uploader::upload_new_artifacts(
                                &proj3, &tid3, &relay3, &tok3, task_started_at, is_code_task,
                            ).await;
                            if !artifacts.is_empty() {
                                let filenames: Vec<String> = artifacts.iter()
                                    .map(|a| a.filename.clone())
                                    .collect();
                                let notify = serde_json::json!({
                                    "type": "artifacts_ready",
                                    "taskId": tid3,
                                    "count": artifacts.len(),
                                    "filenames": filenames,
                                });
                                let _ = write3.lock().await
                                    .send(Message::Text(notify.to_string().into()))
                                    .await;
                            }
                        });
                        break;
                    }
                }
            });
        }

        _ => {}
    }
}
