use std::path::{Path, PathBuf};
use std::time::SystemTime;

const SUPPORTED_EXTENSIONS: &[&str] = &[
    "pptx", "docx", "pdf", "csv", "xlsx", "txt", "md",
    "py", "js", "ts", "zip", "png", "jpg", "jpeg", "svg", "json",
];

const MAX_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB
const MAX_SCAN_DEPTH: usize = 4;
const SECRET_SCAN_LIMIT: usize = 65_536; // scan first 64 KB for secrets

// Directories to skip during scanning
const SKIP_DIRS: &[&str] = &[
    "node_modules", "target", ".git", "__pycache__", ".venv",
    "venv", "dist", "build", ".next", ".expo",
];

// Filenames that must never be uploaded regardless of extension
const BLOCKED_FILENAMES: &[&str] = &[
    ".env", ".env.local", ".env.development", ".env.production",
    ".env.staging", ".env.test", ".env.example",
    "credentials", "secrets", "secret",
    "id_rsa", "id_ed25519", "id_ecdsa", "id_dsa",
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

    // Canonicalize workspace root once; all collected paths are validated against it.
    let canonical_root = match std::fs::canonicalize(&scan_root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[artifacts] failed to canonicalize workspace root: {}", e);
            return Vec::new();
        }
    };

    let files = collect_modified_files(&scan_root, since, MAX_SCAN_DEPTH);
    if files.is_empty() {
        return Vec::new();
    }

    let client = crate::tls_pinning::get_pinned_http_client();
    let mut results = Vec::new();

    for file_path in &files {
        match upload_one(&client, file_path, task_id, project_path, relay_url, token, &canonical_root).await {
            Some(artifact) => {
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

        // Reject symlinks — do not follow them into or out of the workspace.
        let symlink_meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if symlink_meta.file_type().is_symlink() {
            eprintln!("[artifacts] rejected symlink: {:?}", path.file_name());
            continue;
        }

        if symlink_meta.file_type().is_dir() {
            if depth < max_depth {
                collect_recursive(&path, since, max_depth, depth + 1, out);
            }
        } else if symlink_meta.file_type().is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            if !SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
                continue;
            }

            // Block by exact filename regardless of extension
            if BLOCKED_FILENAMES.contains(&name) {
                eprintln!("[artifacts] rejected blocked filename: {}", name);
                continue;
            }

            let size = symlink_meta.len();
            if size == 0 || size > MAX_FILE_SIZE {
                continue;
            }
            if let Ok(modified) = symlink_meta.modified() {
                if modified >= since {
                    out.push(path);
                }
            }
        }
        // Anything else (device, pipe, socket, mount point) is silently skipped.
    }
}

async fn upload_one(
    client: &reqwest::Client,
    path: &Path,
    task_id: &str,
    project_path: &str,
    relay_url: &str,
    token: &str,
    canonical_root: &Path,
) -> Option<UploadedArtifact> {
    let filename = path.file_name()?.to_str()?.to_string();

    // Reject symlinks at upload time (defence-in-depth; collect_recursive also checks).
    let symlink_meta = std::fs::symlink_metadata(path).ok()?;
    if symlink_meta.file_type().is_symlink() {
        eprintln!("[artifacts] rejected symlink at upload: {}", filename);
        return None;
    }

    // Canonicalize and verify the path is contained within the workspace root.
    // This catches any path traversal that slipped past collection-time checks.
    let canonical = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[artifacts] cannot canonicalize {}: {}", filename, e);
            return None;
        }
    };
    if !canonical.starts_with(canonical_root) {
        eprintln!("[artifacts] path traversal rejected: {} is outside workspace", filename);
        return None;
    }

    let bytes = tokio::fs::read(path).await.ok()?;

    // Validate magic bytes match the declared extension for binary formats.
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if let Some(reason) = validate_mime(&ext, &bytes) {
        eprintln!("[artifacts] MIME mismatch for {}: {}", filename, reason);
        return None;
    }

    // Scan for secrets before transmitting.
    if let Some(reason) = scan_for_secrets(&filename, &bytes) {
        eprintln!("[artifacts] secret detected in {}: {}", filename, reason);
        return None;
    }

    // Send only the project directory name (not the full local path) to avoid
    // leaking the user's filesystem layout and username to the relay server.
    let project_name = std::path::Path::new(project_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(project_path);

    let base = relay_url.trim_end_matches('/');
    let upload_url = format!("{base}/artifacts/upload");
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
    // Audit log: record every successful upload for traceability.
    eprintln!("[artifacts] uploaded {} → artifact {}", filename, artifact_id);
    Some(UploadedArtifact { filename, artifact_id })
}

/// Validate that file magic bytes match the declared extension.
/// Returns `Some(reason)` on mismatch. Text-based formats are not checked.
fn validate_mime(ext: &str, bytes: &[u8]) -> Option<String> {
    match ext {
        "png" => {
            if bytes.len() < 4 || &bytes[..4] != b"\x89PNG" {
                return Some("not a valid PNG (magic bytes mismatch)".into());
            }
        }
        "jpg" | "jpeg" => {
            if bytes.len() < 3 || &bytes[..3] != b"\xff\xd8\xff" {
                return Some("not a valid JPEG (magic bytes mismatch)".into());
            }
        }
        "pdf" => {
            if bytes.len() < 4 || &bytes[..4] != b"%PDF" {
                return Some("not a valid PDF (magic bytes mismatch)".into());
            }
        }
        "zip" | "docx" | "pptx" | "xlsx" => {
            // Office Open XML formats are ZIP-based; accept both normal and empty-zip signatures.
            let is_zip = bytes.len() >= 4
                && (&bytes[..4] == b"PK\x03\x04" || &bytes[..4] == b"PK\x05\x06");
            if !is_zip {
                return Some(format!("not a valid ZIP-based file for .{ext} (magic bytes mismatch)"));
            }
        }
        // txt, md, py, js, ts, json, csv, svg are text — no reliable magic bytes.
        _ => {}
    }
    None
}

/// Scan the first `SECRET_SCAN_LIMIT` bytes of a file for high-confidence secret patterns.
/// Returns `Some(description)` if a secret is detected.
/// Binary formats (images, archives, office docs) are not scanned — they cannot embed
/// plain-text secrets in a way that would be useful to an attacker.
fn scan_for_secrets(filename: &str, bytes: &[u8]) -> Option<String> {
    let lower = filename.to_lowercase();
    let is_binary = lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".zip")
        || lower.ends_with(".docx")
        || lower.ends_with(".pptx")
        || lower.ends_with(".xlsx")
        || lower.ends_with(".pdf");
    if is_binary {
        return None;
    }

    let scan_len = bytes.len().min(SECRET_SCAN_LIMIT);
    // If the file isn't valid UTF-8 in the first chunk treat it as binary.
    let content = match std::str::from_utf8(&bytes[..scan_len]) {
        Ok(s) => s,
        Err(_) => return None,
    };

    // SSH / TLS private key headers
    if content.contains("-----BEGIN RSA PRIVATE KEY-----")
        || content.contains("-----BEGIN OPENSSH PRIVATE KEY-----")
        || content.contains("-----BEGIN EC PRIVATE KEY-----")
        || content.contains("-----BEGIN DSA PRIVATE KEY-----")
        || content.contains("-----BEGIN PRIVATE KEY-----")
    {
        return Some("SSH/TLS private key detected".into());
    }

    // AWS access key ID: AKIA followed by exactly 16 uppercase-alphanumeric characters
    if contains_aws_key(content) {
        return Some("AWS access key ID detected".into());
    }

    // High-confidence token prefixes that are specific to credential formats
    if contains_known_token_prefix(content) {
        return Some("credential token prefix detected".into());
    }

    None
}

/// Detect AWS access key IDs: `AKIA` + 16 uppercase-alphanumeric characters (total 20 chars).
fn contains_aws_key(content: &str) -> bool {
    let b = content.as_bytes();
    let n = b.len();
    if n < 20 {
        return false;
    }
    for i in 0..=(n - 20) {
        if &b[i..i + 4] == b"AKIA" {
            let suffix = &b[i + 4..i + 20];
            if suffix.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

/// Detect high-confidence, format-specific token prefixes that are extremely unlikely
/// to appear in legitimate source or document files.
fn contains_known_token_prefix(content: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk-ant-",   // Anthropic API key
        "sk-proj-",  // OpenAI project key
        "ghp_",      // GitHub personal access token
        "gho_",      // GitHub OAuth token
        "ghs_",      // GitHub service token
        "ghr_",      // GitHub refresh token
        "xoxb-",     // Slack bot token
        "xoxp-",     // Slack user token
        "xoxa-",     // Slack app-level token
        "xoxr-",     // Slack refresh token
    ];
    PREFIXES.iter().any(|p| content.contains(p))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir_path(suffix: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("artifact_test_{}_{}", std::process::id(), suffix));
        p
    }

    // ── Path traversal ────────────────────────────────────────────────────────

    #[test]
    fn test_path_containment_inside_workspace() {
        let ws = temp_dir_path("ws_inside");
        fs::create_dir_all(&ws).unwrap();
        let file = ws.join("report.txt");
        fs::write(&file, b"hello").unwrap();

        let canonical_root = fs::canonicalize(&ws).unwrap();
        let canonical_file = fs::canonicalize(&file).unwrap();
        assert!(canonical_file.starts_with(&canonical_root));

        fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn test_path_containment_outside_workspace_rejected() {
        let ws = temp_dir_path("ws_outside");
        fs::create_dir_all(&ws).unwrap();

        let outside = temp_dir_path("outside.txt");
        fs::write(&outside, b"escaped").unwrap();

        let canonical_root = fs::canonicalize(&ws).unwrap();
        let canonical_outside = fs::canonicalize(&outside).unwrap();
        assert!(!canonical_outside.starts_with(&canonical_root));

        fs::remove_dir_all(&ws).ok();
        fs::remove_file(&outside).ok();
    }

    // ── Symlink detection ─────────────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn test_symlink_detected_via_symlink_metadata() {
        let ws = temp_dir_path("ws_symlink");
        fs::create_dir_all(&ws).unwrap();
        let real = ws.join("real.txt");
        fs::write(&real, b"content").unwrap();
        let link = ws.join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let meta = fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink());

        fs::remove_dir_all(&ws).ok();
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_escape_to_parent_rejected() {
        let ws = temp_dir_path("ws_escape");
        fs::create_dir_all(&ws).unwrap();
        // Symlink pointing outside the workspace
        let link = ws.join("escape.txt");
        std::os::unix::fs::symlink("/etc/passwd", &link).unwrap();

        let meta = fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink(), "must be detected as symlink");

        fs::remove_dir_all(&ws).ok();
    }

    // ── MIME / magic byte validation ─────────────────────────────────────────

    #[test]
    fn test_mime_valid_png() {
        assert!(validate_mime("png", b"\x89PNG\r\n\x1a\n").is_none());
    }

    #[test]
    fn test_mime_invalid_png() {
        assert!(validate_mime("png", b"GIF89a...").is_some());
    }

    #[test]
    fn test_mime_valid_jpeg() {
        assert!(validate_mime("jpg", &[0xff, 0xd8, 0xff, 0xe0]).is_none());
        assert!(validate_mime("jpeg", &[0xff, 0xd8, 0xff, 0xe1]).is_none());
    }

    #[test]
    fn test_mime_invalid_jpeg() {
        assert!(validate_mime("jpg", b"not a jpeg").is_some());
    }

    #[test]
    fn test_mime_valid_pdf() {
        assert!(validate_mime("pdf", b"%PDF-1.7 rest").is_none());
    }

    #[test]
    fn test_mime_invalid_pdf() {
        assert!(validate_mime("pdf", b"<html>not a pdf</html>").is_some());
    }

    #[test]
    fn test_mime_valid_zip() {
        assert!(validate_mime("zip", b"PK\x03\x04...").is_none());
        assert!(validate_mime("docx", b"PK\x03\x04...").is_none());
        assert!(validate_mime("xlsx", b"PK\x03\x04...").is_none());
        assert!(validate_mime("pptx", b"PK\x03\x04...").is_none());
    }

    #[test]
    fn test_mime_invalid_zip() {
        assert!(validate_mime("zip", b"Rar!...").is_some());
    }

    #[test]
    fn test_mime_text_formats_not_checked() {
        // Text formats have no magic bytes — should always pass
        assert!(validate_mime("txt", b"anything").is_none());
        assert!(validate_mime("md", b"# heading").is_none());
        assert!(validate_mime("json", b"{}").is_none());
        assert!(validate_mime("py", b"import os").is_none());
        assert!(validate_mime("svg", b"<svg>").is_none());
    }

    // ── Secret scanning ───────────────────────────────────────────────────────

    #[test]
    fn test_secret_rsa_private_key() {
        let content = b"-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA";
        assert!(scan_for_secrets("key.txt", content).is_some());
    }

    #[test]
    fn test_secret_openssh_private_key() {
        let content = b"-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk";
        assert!(scan_for_secrets("id_ed25519.pub", content).is_some());
    }

    #[test]
    fn test_secret_aws_access_key() {
        let content = b"aws_access_key_id = AKIAIOSFODNN7EXAMPLE\n";
        assert!(scan_for_secrets("config.txt", content).is_some());
    }

    #[test]
    fn test_secret_aws_key_not_triggered_on_short() {
        // 15 chars after AKIA — not a valid 20-char key
        let content = b"AKIA123456789012345 rest";
        assert!(scan_for_secrets("file.txt", content).is_none());
    }

    #[test]
    fn test_secret_github_token() {
        let content = b"GITHUB_TOKEN=ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234\n";
        assert!(scan_for_secrets("env.txt", content).is_some());
    }

    #[test]
    fn test_secret_anthropic_key() {
        let content = b"api_key = sk-ant-api03-XXXXXXXXXXXX\n";
        assert!(scan_for_secrets("config.json", content).is_some());
    }

    #[test]
    fn test_secret_slack_token() {
        let content = b"SLACK_TOKEN=xoxb-12345-67890-abcdefgh\n";
        assert!(scan_for_secrets("config.txt", content).is_some());
    }

    #[test]
    fn test_secret_clean_file() {
        let content = b"Hello world. This is a normal report with no secrets.";
        assert!(scan_for_secrets("report.txt", content).is_none());
    }

    #[test]
    fn test_secret_binary_formats_skipped() {
        // Even if binary content contains a pattern, binary files are not scanned.
        let mut content = Vec::new();
        content.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        content.extend_from_slice(b"AKIAIOSFODNN7EXAMPLE embedded");
        assert!(scan_for_secrets("image.png", &content).is_none());
    }

    #[test]
    fn test_secret_non_utf8_treated_as_binary() {
        // Non-UTF-8 content that happens to contain bytes resembling a key is skipped.
        let mut content = vec![0xff, 0xfe];
        content.extend_from_slice(b"AKIAIOSFODNN7EXAMPLE");
        assert!(scan_for_secrets("data.bin", &content).is_none());
    }

    // ── Oversized upload ──────────────────────────────────────────────────────

    #[test]
    fn test_max_file_size_constant() {
        assert_eq!(MAX_FILE_SIZE, 50 * 1024 * 1024);
    }

    // ── Blocked filenames ─────────────────────────────────────────────────────

    #[test]
    fn test_blocked_filename_env() {
        assert!(BLOCKED_FILENAMES.contains(&".env"));
        assert!(BLOCKED_FILENAMES.contains(&".env.local"));
        assert!(BLOCKED_FILENAMES.contains(&".env.production"));
    }

    #[test]
    fn test_blocked_filename_ssh_keys() {
        assert!(BLOCKED_FILENAMES.contains(&"id_rsa"));
        assert!(BLOCKED_FILENAMES.contains(&"id_ed25519"));
        assert!(BLOCKED_FILENAMES.contains(&"id_ecdsa"));
    }

    #[test]
    fn test_blocked_filename_credentials() {
        assert!(BLOCKED_FILENAMES.contains(&"credentials"));
        assert!(BLOCKED_FILENAMES.contains(&"secrets"));
    }

    // ── AWS key detection edge cases ──────────────────────────────────────────

    #[test]
    fn test_aws_key_exact_20_chars() {
        assert!(contains_aws_key("AKIAIOSFODNN7EXAMPLE rest"));
    }

    #[test]
    fn test_aws_key_no_match_lowercase_suffix() {
        // lowercase chars in suffix → not an AWS key
        assert!(!contains_aws_key("AKIAiosfodnn7example rest"));
    }

    #[test]
    fn test_aws_key_no_match_content_too_short() {
        assert!(!contains_aws_key("AKIA1234"));
    }

    #[test]
    fn test_aws_key_no_match_clean_content() {
        assert!(!contains_aws_key("no credentials here"));
    }
}
