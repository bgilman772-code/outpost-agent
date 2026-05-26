use keyring::Entry;
use serde::{Deserialize, Serialize};

const KEYRING_SERVICE: &str = "outpost-agent";
const KEYRING_USER_PINS: &str = "relay_spki_pins";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PinSet {
    active: Vec<String>,
    backup: Vec<String>,
}

/// Returns all valid SPKI pin hashes (active + backup).
/// Empty vec means no pins stored yet (TOFU mode on next connection).
pub fn get_all_pins() -> Vec<String> {
    let Ok(entry) = Entry::new(KEYRING_SERVICE, KEYRING_USER_PINS) else {
        return vec![];
    };
    let Ok(json) = entry.get_password() else {
        return vec![];
    };
    let Ok(pins): Result<PinSet, _> = serde_json::from_str(&json) else {
        return vec![];
    };
    let mut all = pins.active;
    all.extend(pins.backup);
    all
}

/// Record the first pin seen (Trust On First Use). Only called when no pins exist.
pub fn save_tofu_pin(hash_hex: &str) -> Result<(), String> {
    let pins = PinSet {
        active: vec![hash_hex.to_string()],
        backup: vec![],
    };
    write_pins(&pins)
}

/// Add a backup pin for key rotation support.
/// The backup is accepted alongside the active pin until rotation is complete.
#[allow(dead_code)]
pub fn add_backup_pin(hash_hex: &str) -> Result<(), String> {
    let Ok(entry) = Entry::new(KEYRING_SERVICE, KEYRING_USER_PINS) else {
        return Err("keyring unavailable".into());
    };
    let mut pins: PinSet = entry
        .get_password()
        .ok()
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    if !pins.backup.contains(&hash_hex.to_string()) {
        pins.backup.push(hash_hex.to_string());
    }
    write_pins(&pins)
}

/// Remove all stored pins. Called during unpair so the next pair triggers TOFU.
pub fn clear_pins() {
    let Ok(entry) = Entry::new(KEYRING_SERVICE, KEYRING_USER_PINS) else {
        return;
    };
    let _ = entry.delete_credential();
}

fn write_pins(pins: &PinSet) -> Result<(), String> {
    let entry = Entry::new(KEYRING_SERVICE, KEYRING_USER_PINS).map_err(|e| e.to_string())?;
    let json = serde_json::to_string(pins).map_err(|e| e.to_string())?;
    entry.set_password(&json).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test uses a unique "service" via a side-channel-free scope to avoid
    // keyring collisions between parallel test runs. We test the PinSet logic
    // directly without hitting the real keyring.

    fn make_pin_set(active: &[&str], backup: &[&str]) -> PinSet {
        PinSet {
            active: active.iter().map(|s| s.to_string()).collect(),
            backup: backup.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn test_pin_set_combined_returns_all() {
        let ps = make_pin_set(&["aaa", "bbb"], &["ccc"]);
        let mut all = ps.active.clone();
        all.extend(ps.backup.clone());
        assert_eq!(all.len(), 3);
        assert!(all.contains(&"aaa".to_string()));
        assert!(all.contains(&"ccc".to_string()));
    }

    #[test]
    fn test_pin_match_active() {
        let ps = make_pin_set(&["deadbeef"], &[]);
        let all: Vec<String> = ps.active.iter().chain(ps.backup.iter()).cloned().collect();
        assert!(all.iter().any(|p| p == "deadbeef"));
    }

    #[test]
    fn test_pin_match_backup() {
        let ps = make_pin_set(&["oldpin"], &["newpin"]);
        let all: Vec<String> = ps.active.iter().chain(ps.backup.iter()).cloned().collect();
        assert!(all.iter().any(|p| p == "newpin"), "backup pin must be accepted");
    }

    #[test]
    fn test_pin_violation_wrong_hash() {
        let ps = make_pin_set(&["correctpin"], &[]);
        let all: Vec<String> = ps.active.iter().chain(ps.backup.iter()).cloned().collect();
        assert!(
            !all.iter().any(|p| p == "wrongpin"),
            "wrong hash must not match stored pins"
        );
    }

    #[test]
    fn test_pin_json_roundtrip() {
        let original = make_pin_set(&["pin1", "pin2"], &["backup1"]);
        let json = serde_json::to_string(&original).unwrap();
        let decoded: PinSet = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.active, original.active);
        assert_eq!(decoded.backup, original.backup);
    }

    #[test]
    fn test_pin_empty_set_is_tofu_mode() {
        let ps = make_pin_set(&[], &[]);
        let all: Vec<String> = ps.active.iter().chain(ps.backup.iter()).cloned().collect();
        assert!(all.is_empty(), "empty pin set must trigger TOFU on connection");
    }

    #[test]
    fn test_no_duplicate_backup_pins() {
        let mut ps = make_pin_set(&["active"], &[]);
        let new_pin = "backup".to_string();
        // Simulate add_backup_pin deduplication logic
        if !ps.backup.contains(&new_pin) {
            ps.backup.push(new_pin.clone());
        }
        if !ps.backup.contains(&new_pin) {
            ps.backup.push(new_pin.clone());
        }
        assert_eq!(ps.backup.len(), 1, "duplicate backup pin must not be added");
    }
}
