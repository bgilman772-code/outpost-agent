use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Every capability an agent action can require.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Capability {
    FilesystemRead,
    FilesystemWrite,
    ShellExecute,
    GitRead,
    GitWrite,
    ArtifactUpload,
    AIExecution,
    NetworkAccess,
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::FilesystemRead  => "filesystem_read",
            Self::FilesystemWrite => "filesystem_write",
            Self::ShellExecute    => "shell_execute",
            Self::GitRead         => "git_read",
            Self::GitWrite        => "git_write",
            Self::ArtifactUpload  => "artifact_upload",
            Self::AIExecution     => "ai_execution",
            Self::NetworkAccess   => "network_access",
        };
        write!(f, "{s}")
    }
}

/// Returns the capability set for a message type, or `None` if the type is not on
/// the allowlist and must be rejected (deny-by-default).
pub fn message_capabilities(msg_type: &str) -> Option<&'static [Capability]> {
    use Capability::*;
    static NONE:           &[Capability] = &[];
    static PROBE:          &[Capability] = &[FilesystemRead];
    static SETUP_PROJECT:  &[Capability] = &[FilesystemWrite];
    static LIST_FILES:     &[Capability] = &[FilesystemRead];
    static LIST_DIRS:      &[Capability] = &[FilesystemRead];
    static CREATE_PROJECT: &[Capability] = &[FilesystemWrite];
    static CLONE_REPO:     &[Capability] = &[FilesystemWrite, NetworkAccess, GitRead, GitWrite];
    static GIT_PUSH:       &[Capability] = &[FilesystemRead, GitRead, GitWrite, NetworkAccess];
    static RUN_TASK:       &[Capability] = &[FilesystemRead, FilesystemWrite, ShellExecute, AIExecution, ArtifactUpload];
    static SETUP_OLLAMA:   &[Capability] = &[FilesystemWrite, NetworkAccess, ShellExecute];

    match msg_type {
        "registered"         => Some(NONE),
        "probe"              => Some(PROBE),
        "setup_project"      => Some(SETUP_PROJECT),
        "list_project_files" => Some(LIST_FILES),
        "list_directories"   => Some(LIST_DIRS),
        "create_project"     => Some(CREATE_PROJECT),
        "clone_repo"         => Some(CLONE_REPO),
        "git_push"           => Some(GIT_PUSH),
        "run_task"           => Some(RUN_TASK),
        "setup_ollama"       => Some(SETUP_OLLAMA),
        _                    => None,
    }
}

/// Returns true if `msg_type` requires explicit user approval before execution.
/// Currently: git_push (pushes code externally; highest exfiltration risk).
pub fn requires_approval(msg_type: &str) -> bool {
    matches!(msg_type, "git_push")
}

/// Verify HMAC-SHA256(key=token, msg=payload_str) against the provided hex sig.
/// Uses a fold-based constant-time comparison to prevent timing side-channels.
pub fn verify_command_sig(token: &str, payload_str: &str, sig: &str) -> bool {
    type HmacSha256 = Hmac<Sha256>;
    let Ok(mut mac) = HmacSha256::new_from_slice(token.as_bytes()) else {
        return false;
    };
    mac.update(payload_str.as_bytes());
    let expected = mac.finalize().into_bytes();
    let expected_hex = hex::encode(expected);
    if expected_hex.len() != sig.len() {
        return false;
    }
    // Constant-time byte comparison via folded XOR
    expected_hex
        .as_bytes()
        .iter()
        .zip(sig.as_bytes().iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Emit a structured audit log line for every capability check.
pub fn audit_log(msg_type: &str, caps: &[Capability], allowed: bool, reason: Option<&str>) {
    let cap_list: Vec<String> = caps.iter().map(|c| c.to_string()).collect();
    eprintln!(
        "[cap-audit] action={msg_type} caps=[{}] allowed={allowed} reason={}",
        cap_list.join(","),
        reason.unwrap_or("ok"),
    );
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_message_types_are_allowed() {
        for t in &[
            "registered", "probe", "setup_project", "list_project_files",
            "list_directories", "create_project", "clone_repo",
            "git_push", "run_task", "setup_ollama",
        ] {
            assert!(message_capabilities(t).is_some(), "expected {t} to be allowed");
        }
    }

    #[test]
    fn test_malicious_relay_commands_rejected() {
        for t in &[
            "inject_payload", "exec_shell", "system_exec", "bootstrap_register",
            "rm_rf", "eval", "__proto__", "cmd_override", "",
        ] {
            assert!(
                message_capabilities(t).is_none(),
                "expected '{t}' to be denied (not on allowlist)"
            );
        }
    }

    #[test]
    fn test_run_task_has_required_capabilities() {
        let caps = message_capabilities("run_task").unwrap();
        assert!(caps.contains(&Capability::ShellExecute));
        assert!(caps.contains(&Capability::AIExecution));
        assert!(caps.contains(&Capability::FilesystemWrite));
        assert!(caps.contains(&Capability::ArtifactUpload));
    }

    #[test]
    fn test_git_push_has_required_capabilities() {
        let caps = message_capabilities("git_push").unwrap();
        assert!(caps.contains(&Capability::GitWrite));
        assert!(caps.contains(&Capability::NetworkAccess));
        assert!(caps.contains(&Capability::GitRead));
    }

    #[test]
    fn test_git_push_requires_approval() {
        assert!(requires_approval("git_push"));
    }

    #[test]
    fn test_other_types_do_not_require_approval() {
        for t in &["probe", "run_task", "setup_ollama", "clone_repo", "list_directories"] {
            assert!(!requires_approval(t), "{t} should not require approval");
        }
    }

    #[test]
    fn test_verify_sig_valid() {
        let token = "test-agent-token-abc123";
        let payload = r#"{"type":"run_task","taskId":"xyz"}"#;
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(token.as_bytes()).unwrap();
        mac.update(payload.as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());
        assert!(verify_command_sig(token, payload, &sig));
    }

    #[test]
    fn test_verify_sig_wrong_token_rejected() {
        let token = "correct_token";
        let payload = r#"{"type":"run_task"}"#;
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(token.as_bytes()).unwrap();
        mac.update(payload.as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());
        assert!(!verify_command_sig("wrong_token", payload, &sig));
    }

    #[test]
    fn test_verify_sig_tampered_payload_rejected() {
        let token = "mytoken";
        let real_payload = r#"{"type":"probe"}"#;
        let tampered    = r#"{"type":"run_task"}"#;
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(token.as_bytes()).unwrap();
        mac.update(real_payload.as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());
        assert!(!verify_command_sig(token, tampered, &sig));
    }

    #[test]
    fn test_verify_sig_empty_sig_rejected() {
        assert!(!verify_command_sig("token", r#"{"type":"probe"}"#, ""));
    }

    #[test]
    fn test_verify_sig_malformed_sig_rejected() {
        assert!(!verify_command_sig("token", r#"{"type":"probe"}"#, "not-valid-hex!@#$"));
    }

    #[test]
    fn test_verify_sig_length_mismatch_rejected() {
        assert!(!verify_command_sig("token", r#"{"type":"probe"}"#, "abc"));
    }

    #[test]
    fn test_capability_display_names() {
        assert_eq!(Capability::FilesystemRead.to_string(),  "filesystem_read");
        assert_eq!(Capability::FilesystemWrite.to_string(), "filesystem_write");
        assert_eq!(Capability::ShellExecute.to_string(),    "shell_execute");
        assert_eq!(Capability::GitRead.to_string(),         "git_read");
        assert_eq!(Capability::GitWrite.to_string(),        "git_write");
        assert_eq!(Capability::ArtifactUpload.to_string(),  "artifact_upload");
        assert_eq!(Capability::AIExecution.to_string(),     "ai_execution");
        assert_eq!(Capability::NetworkAccess.to_string(),   "network_access");
    }

    #[test]
    fn test_simulated_malicious_unsigned_message() {
        // An attacker injecting a bare {"type":"git_push"} is not on the cmd envelope.
        // The outer handler rejects anything that isn't "registered" or "cmd".
        // This test confirms the capability system also rejects unknown envelope types.
        assert!(message_capabilities("git_push_override").is_none());
        assert!(message_capabilities("run_task_admin").is_none());
    }
}
