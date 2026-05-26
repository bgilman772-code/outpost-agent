use std::path::{Path, PathBuf};
use std::time::SystemTime;

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "pptx", "docx", "pdf", "csv", "xlsx", "txt", "md",
    "py", "js", "ts", "zip", "png", "jpg", "jpeg", "svg", "json",
];

const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB
const MAX_SCAN_DEPTH: usize = 4;

// Directories to skip during scanning
const SKIP_DIRS: &[&str] = &[
    "node_modules", "target", ".git", "__pycache__", ".venv",
    "venv", "dist", "build", ".next", ".expo",
];

#[allow(dead_code)]
pub struct UploadedArtifact {
    pub filename: String,
    pub artifact_id: String,
}

/// Scan `project_path` for files modified at or after `since`, then upload each to the relay.
pub async fn upload_new_artifacts(
    project_path: &str,
    task_id: &str,
    relay_url: &str,
    token: &str,
    since: SystemTime,
    output_only: bool,
) -> Vec<UploadedArtifact> {
    let scan_root = if output_only {
        Path::new(project_path).join("outpost-output")
    } else {
        PathBuf::from(project_path)
    };

    eprintln!("[artifacts] scanning {} for files newer than {:?}", scan_root.display(), since);
    let files = collect_modified_files(&scan_root, since, MAX_SCAN_DEPTH);
    eprintln!("[artifacts] found {} candidate file(s) to upload", files.len());
    if files.is_empty() {
        return Vec::new();
    }

    let client = reqwest::Client::new();
    let mut results = Vec::new();

    for file_path in &files {
        match upload_one(&client, file_path, task_id, project_path, relay_url, token).await {
            Some(artifact) => {
                eprintln!("[artifacts] uploaded {}", artifact.filename);
                results.push(artifact);
            }
            None => {
                eprintln!("[artifacts] failed to upload {:?}", file_path.file_name());
            }
        }
    }

    results
}

fn collect_modified_files(base: &Path, since: SystemTime, max_depth: usize) -> Vec<PathBuf> {
    if !base.exists() || !base.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    collect_recursive(base, since, max_depth, 0, &mut out);
    out
}

fn collect_recursive(
    dir: &Path,
    since: SystemTime,
    max_depth: usize,
    depth: usize,
    out: &mut Vec<PathBuf>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        // Skip hidden entries and known-noisy dirs
        if name.starts_with('.') || SKIP_DIRS.contains(&name) {
            continue;
        }

        if path.is_dir() {
            if depth < max_depth {
                collect_recursive(&path, since, max_depth, depth + 1, out);
            }
        } else if path.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            if !SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
                continue;
            }

            if let Ok(meta) = std::fs::metadata(&path) {
                let size = meta.len();
                if size == 0 || size > MAX_FILE_SIZE {
                    continue;
                }
                if let Ok(modified) = meta.modified() {
                    if modified >= since {
                        out.push(path);
                    }
                }
            }
        }
    }
}

async fn upload_one(
    client: &reqwest::Client,
    path: &Path,
    task_id: &str,
    project_path: &str,
    relay_url: &str,
    token: &str,
) -> Option<UploadedArtifact> {
    let filename = path.file_name()?.to_str()?.to_string();
    let bytes = tokio::fs::read(path).await.ok()?;

    // Send only the project directory name (not the full local path) to avoid
    // leaking the user's filesystem layout and username to the relay server.
    let project_name = std::path::Path::new(project_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(project_path);

    let base = relay_url.trim_end_matches('/');
    let upload_url = format!("{base}/artifacts/upload");
    eprintln!("[artifacts] uploading {} to {}", filename, upload_url);
    let send_result = client
        .post(&upload_url)
        .query(&[("taskId", task_id), ("projectPath", project_name)])
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/octet-stream")
        .header("X-Filename", urlencoded_filename(&filename))
        .body(bytes)
        .send()
        .await;
    let resp = match send_result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[artifacts] upload network error for {}: {}", filename, e);
            return None;
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        eprintln!("[artifacts] upload error {status}: {body}");
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    let artifact_id = body["id"].as_str()?.to_string();
    Some(UploadedArtifact { filename, artifact_id })
}

/// Percent-encode a filename for use in an HTTP header value.
fn urlencoded_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '~') {
                c.to_string()
            } else {
                let bytes = c.to_string();
                bytes
                    .as_bytes()
                    .iter()
                    .map(|b| format!("%{b:02X}"))
                    .collect()
            }
        })
        .collect()
}
