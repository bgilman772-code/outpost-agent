fn main() {
    // SECURITY: VITE_BOOTSTRAP_TOKEN is intentionally NOT injected here.
    //
    // The bootstrap token equals RELAY_SECRET on the relay server. Embedding it
    // in the compiled binary is equivalent to shipping a long-lived admin credential
    // that anyone can extract by reverse-engineering the binary.
    //
    // The agent uses a QR/code pairing flow instead:
    //   1. Agent asks the relay for a short-lived desktop pairing link.
    //   2. User scans that QR code or enters its code in the phone app.
    //   3. Relay returns an agent token only after an authenticated phone claims it.
    //   4. Codes expire quickly and are single-use.
    //
    // RELAY_URL is not a secret (it is a public endpoint address) so it is still
    // baked in as a default to avoid requiring manual entry on every install.
    // Users can always override it at runtime via the agent UI.
    let env_content = std::fs::read_to_string(std::path::Path::new("../.env")).ok();
    let relay_url = std::env::var("VITE_RELAY_URL").unwrap_or_else(|_| {
        if let Some(content) = &env_content {
            for line in content.lines() {
                if let Some(val) = line.strip_prefix("VITE_RELAY_URL=") {
                    return val.trim().to_string();
                }
            }
        }
        String::new()
    });
    println!("cargo:rustc-env=VITE_RELAY_URL={relay_url}");
    println!("cargo:rerun-if-changed=../.env");
    tauri_build::build()
}
