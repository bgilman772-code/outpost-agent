fn main() {
    // Read VITE_RELAY_URL / VITE_BOOTSTRAP_TOKEN from environment, falling back to ../.env file
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
    let bootstrap_token = std::env::var("VITE_BOOTSTRAP_TOKEN").unwrap_or_else(|_| {
        if let Some(content) = &env_content {
            for line in content.lines() {
                if let Some(val) = line.strip_prefix("VITE_BOOTSTRAP_TOKEN=") {
                    return val.trim().to_string();
                }
                if let Some(val) = line.strip_prefix("RELAY_SECRET=") {
                    return val.trim().to_string();
                }
            }
        }
        String::new()
    });
    println!("cargo:rustc-env=VITE_RELAY_URL={relay_url}");
    println!("cargo:rustc-env=VITE_BOOTSTRAP_TOKEN={bootstrap_token}");
    println!("cargo:rerun-if-changed=../.env");
    tauri_build::build()
}
