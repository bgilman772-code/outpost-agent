# Outpost Agent

The Windows desktop agent for [Outpost](https://outpost.up.railway.app) — connects your PC to the Outpost iOS app so you can run AI coding tasks from your phone.

## What it does

- Runs as a lightweight background app on your Windows PC
- Connects to your Outpost relay server via WebSocket
- Receives tasks from your iPhone and runs them locally using Claude CLI
- Uploads output files back to your phone automatically

## Building from source

This is a [Tauri](https://tauri.app) app written in Rust + TypeScript.

**Prerequisites**
- [Rust](https://rustup.rs) (stable)
- [Node.js](https://nodejs.org) 20+
- [Claude CLI](https://github.com/anthropics/claude-code) installed and authenticated

**Setup**

```bash
npm install
cp .env.example .env
# Edit .env with your relay URL and bootstrap token
```

**Development**

```bash
npm run tauri dev
```

**Production build**

```bash
npm run tauri build -- --no-bundle
# Output: src-tauri/target/release/outpost-agent.exe
```

## Configuration

Copy `.env.example` to `.env` and fill in:

| Variable | Description |
|---|---|
| `VITE_RELAY_URL` | URL of your deployed Outpost relay server |
| `VITE_BOOTSTRAP_TOKEN` | Must match `RELAY_SECRET` on your relay server |

These values are compiled into the binary at build time. The `.env` file is gitignored and should never be committed.

## Architecture

The agent connects to the relay's `/agent` WebSocket endpoint using a token obtained during the initial pairing flow. Once connected it responds to messages from the relay:

- `run_task` — runs a Claude CLI task in a project directory, streams output back
- `probe` — reports installed tools, projects, and system info
- `setup_ollama` — installs and configures a local Ollama model
- `clone_repo`, `git_push`, `create_project` — project management operations

All sensitive credentials (API keys, tokens) are stored in the OS keychain via Tauri's secure storage — never written to disk in plaintext.

## License

MIT
