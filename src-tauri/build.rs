fn main() {
    // SECURITY: VITE_BOOTSTRAP_TOKEN is intentionally NOT injected here.
    //
    // The bootstrap token equals RELAY_SECRET on the relay server. Embedding it
    // in the compiled binary is equivalent to shipping a long-lived admin credential
    // that anyone can extract by reverse-engineering the binary.
    //
    // The agent uses the QR/code pairing flow instead:
    //   1. User opens the phone app → taps "Add PC" → sees a one-time code.
    //   2. User enters the code in the agent desktop app.
    //   3. Agent calls POST /agent/pair/:code/claim — no embedded secret required.
    //   4. Codes expire in 10 minutes and are single-use.
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
