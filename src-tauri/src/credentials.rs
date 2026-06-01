use keyring::Entry;

const SERVICE: &str = "outpost-relay";
const TOKEN_ACCOUNT: &str = "relay-token";
const REFRESH_ACCOUNT: &str = "relay-refresh-token";

// ── Access token ─────────────────────────────────────────────────────────────

/// Store the relay access token in OS-native secure credential storage.
pub fn store_token(token: &str) -> Result<(), String> {
    Entry::new(SERVICE, TOKEN_ACCOUNT)
        .and_then(|e| e.set_password(token))
        .map_err(|e| format!("credential store error: {e}"))
}

/// Load the relay access token from the OS credential store.
/// Returns `Ok(None)` if no entry exists (not an error — just unpaired).
/// Returns `Err` only when the store itself is inaccessible.
pub fn load_token() -> Result<Option<String>, String> {
    match Entry::new(SERVICE, TOKEN_ACCOUNT).and_then(|e| e.get_password()) {
        Ok(t) if t.is_empty() => Ok(None),
        Ok(t) => Ok(Some(t)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("credential store error: {e}")),
    }
}

/// Remove the relay access token from the OS credential store.
pub fn delete_token() -> Result<(), String> {
    match Entry::new(SERVICE, TOKEN_ACCOUNT).and_then(|e| e.delete_credential()) {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()), // idempotent
        Err(e) => Err(format!("credential store error: {e}")),
    }
}

// ── Refresh token ─────────────────────────────────────────────────────────────

pub fn store_refresh_token(token: &str) -> Result<(), String> {
    Entry::new(SERVICE, REFRESH_ACCOUNT)
        .and_then(|e| e.set_password(token))
        .map_err(|e| format!("credential store error: {e}"))
}

#[allow(dead_code)]
pub fn load_refresh_token() -> Result<Option<String>, String> {
    match Entry::new(SERVICE, REFRESH_ACCOUNT).and_then(|e| e.get_password()) {
        Ok(t) if t.is_empty() => Ok(None),
        Ok(t) => Ok(Some(t)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("credential store error: {e}")),
    }
}

pub fn delete_refresh_token() -> Result<(), String> {
    match Entry::new(SERVICE, REFRESH_ACCOUNT).and_then(|e| e.delete_credential()) {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("credential store error: {e}")),
    }
}

// ── Lifecycle helpers ─────────────────────────────────────────────────────────

/// Delete both access and refresh tokens (called on unpair).
pub fn delete_all() {
    if let Err(e) = delete_token() {
        eprintln!("[credentials] failed to delete access token: {e}");
    }
    if let Err(e) = delete_refresh_token() {
        eprintln!("[credentials] failed to delete refresh token: {e}");
    }
}

/// Verify that the OS credential store is reachable.
/// Returns `Ok(())` if operational, `Err(description)` if not.
pub fn check_availability() -> Result<(), String> {
    let entry = Entry::new(SERVICE, "__probe__")
        .map_err(|e| format!("credential store unavailable: {e}"))?;
    match entry.get_password() {
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("credential store unavailable: {e}")),
    }
}

/// Move a legacy plaintext token into secure storage.
/// Returns `true` if migration was performed, `false` if the input was empty.
/// Never panics — failures are logged and the caller decides how to proceed.
pub fn migrate_from_plaintext(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    match store_token(token) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[credentials] migration to keyring failed: {e}");
            false
        }
    }
}

// ── Token rotation ────────────────────────────────────────────────────────────

/// Exchange the stored refresh token for a new access token via the relay.
/// Updates the keyring entry on success.
/// Returns the new access token so callers can reconnect immediately.
#[allow(dead_code)]
pub async fn rotate_token(relay_url: &str) -> Result<String, String> {
    let refresh = load_refresh_token()?
        .ok_or_else(|| "no refresh token stored; re-pair required".to_string())?;

    let url = format!("{}/auth/refresh", relay_url.trim_end_matches('/'));
    let client = crate::tls_pinning::get_pinned_http_client();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "refreshToken": refresh }))
        .send()
        .await
        .map_err(|e| format!("token refresh request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("token refresh failed ({status}): {body}"));
    }

    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let new_token = body["accessToken"]
        .as_str()
        .ok_or("no accessToken in refresh response")?
        .to_string();

    store_token(&new_token)?;

    // Update refresh token if the relay issued a new one.
    if let Some(new_refresh) = body["refreshToken"].as_str() {
        if let Err(e) = store_refresh_token(new_refresh) {
            eprintln!("[credentials] failed to update refresh token: {e}");
        }
    }

    Ok(new_token)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Verify that the AgentConfig serde implementation never emits the token field.
    // This test does not touch the keyring.
    #[test]
    fn test_config_token_not_serialized() {
        use crate::config::AgentConfig;
        let cfg = AgentConfig {
            relay_url: "https://relay.example.com".into(),
            token: "super-secret-token".into(), // legacy field — should not appear in output
            agent_machine_id: "machine-abc".into(),
            hostname: "testhost".into(),
            relay_url_override: String::new(),
            token_issued_at: None,
        };
        let json = serde_json::to_string(&cfg).expect("serialization must not fail");
        assert!(
            !json.contains("super-secret-token"),
            "token value must not appear in serialized JSON: {json}"
        );
        assert!(
            !json.contains("\"token\""),
            "token key must not appear in serialized JSON: {json}"
        );
    }

    // Verify that a legacy config.json containing a plaintext token can be read back
    // so that the migration path works.
    #[test]
    fn test_migration_reads_legacy_token() {
        let json = r#"{
            "relay_url": "https://relay.example.com",
            "token": "legacy-plaintext-token",
            "agent_machine_id": "machine-abc",
            "hostname": "testhost"
        }"#;
        let cfg: crate::config::AgentConfig =
            serde_json::from_str(json).expect("deserialization must succeed");
        assert_eq!(
            cfg.token, "legacy-plaintext-token",
            "migration path must be able to read the legacy token field"
        );
        assert!(cfg.is_paired(), "config with relay_url+machine_id should report paired");
    }

    // Verify that after migration the re-serialized config no longer contains the token.
    #[test]
    fn test_migration_removes_token_on_resave() {
        use crate::config::AgentConfig;
        let mut cfg = AgentConfig {
            relay_url: "https://relay.example.com".into(),
            token: "legacy-plaintext-token".into(),
            agent_machine_id: "machine-abc".into(),
            hostname: "testhost".into(),
            relay_url_override: String::new(),
            token_issued_at: None,
        };
        // Simulate what lib.rs does after migration: clear the in-memory token.
        cfg.token = String::new();
        let json = serde_json::to_string_pretty(&cfg).expect("serialization must not fail");
        assert!(
            !json.contains("legacy-plaintext-token"),
            "migrated config must not contain old token: {json}"
        );
        assert!(
            !json.contains("\"token\""),
            "migrated config must not contain token key: {json}"
        );
    }

    // Verify is_paired() no longer depends on the token field.
    #[test]
    fn test_is_paired_uses_relay_url_and_machine_id() {
        use crate::config::AgentConfig;
        let unpaired = AgentConfig::default();
        assert!(!unpaired.is_paired());

        let paired = AgentConfig {
            relay_url: "https://relay.example.com".into(),
            token: String::new(), // token intentionally absent
            agent_machine_id: "machine-abc".into(),
            hostname: "testhost".into(),
            relay_url_override: String::new(),
            token_issued_at: None,
        };
        assert!(paired.is_paired());
    }

    // Keyring round-trip: store → load → delete.
    // Requires a functioning OS keychain. Skipped in environments without one.
    #[test]
    fn test_keyring_roundtrip() {
        // Use a throwaway service/account to avoid touching real credentials.
        let entry = match Entry::new("outpost-relay-test", "test-token") {
            Ok(e) => e,
            Err(_) => return, // keyring not available in this environment
        };

        // Clean slate
        let _ = entry.delete_credential();

        let secret = "test-secret-abc123";
        entry.set_password(secret).expect("store must succeed");

        let loaded = entry.get_password().expect("load must succeed");
        assert_eq!(loaded, secret);

        entry.delete_credential().expect("delete must succeed");

        // Verify deletion
        match entry.get_password() {
            Err(keyring::Error::NoEntry) => {}
            other => panic!("expected NoEntry after delete, got: {other:?}"),
        }
    }
}
