# AIS Runner

A desktop tool for running and testing **Azure Logic Apps Standard** workflows locally, built with [Dioxus](https://dioxuslabs.com/) (Rust).

## What it does

- **Start / stop** Azurite and `func start` from a single UI
- **Browse** all 100+ workflows in your `logic_apps/` folder, with health status and trigger type
- **Filter** the workflow list by name
- **View source** (`workflow.json`) for any workflow with one click
- **Run** any workflow with an auto-generated JSON payload (derived from the trigger schema), editable before sending
- **Live polling** — action timeline updates in real time until the run completes
- **Settings editor** — edit `local.settings.json` key-value pairs, with Azure CLI integration to fetch Service Bus connection strings and browse subscriptions/namespaces
- **Tool check** — warns on launch if `func`, `azurite`, `az`, or `node` are missing from PATH

---

## Install

### macOS (Apple Silicon)

```bash
brew tap Bennekrouf/aisrunner
brew install aisrunner
```

This installs `ais-runner` plus its dependencies (Node.js, Azure CLI, Azurite, Azure Functions Core Tools).

### Windows

1. Download `ais-runner-windows.zip` from the [latest release](https://github.com/Bennekrouf/ais-runner/releases/latest)
2. Extract the ZIP anywhere
3. Right-click `setup-windows.ps1` → **Run with PowerShell** (first time only — installs Node.js, Azure CLI, Azurite, func)
4. Run `ais-runner.exe`

---

## Usage

1. Launch `ais-runner`
2. Select your `logic_apps/` folder
3. The app starts Azurite and `func start`, then lists all discovered workflows
4. Click any workflow to view its source, run it, or inspect its action timeline

Configuration is stored in:
- **macOS:** `~/.config/ais-runner/config.json`
- **Windows:** `%APPDATA%\ais-runner\config.json`

---

## Contributing

### Prerequisites

| Tool | Install |
|------|---------|
| **Rust** | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| **Dioxus CLI** (optional) | `cargo install dioxus-cli` |

### Build from source

```bash
git clone https://github.com/Bennekrouf/ais-runner.git
cd ais-runner
cargo build --release
./target/release/ais-runner
```

### Project structure

| Path | Description |
|------|-------------|
| `src/main.rs` | App entry point and UI |
| `src/utils.rs` | Helper functions |
| `scripts/setup-mac.sh` | macOS dependency installer |
| `scripts/setup-windows.ps1` | Windows dependency installer |
| `.github/workflows/` | CI: Mac build, Windows build, release pipeline |

### Release process

Releases are fully automated. To publish a new version:

```bash
# 1. Bump version in Cargo.toml
# 2. Commit and tag
git tag v0.2.0
git push origin v0.2.0
```

This triggers the release workflow which:
- Builds macOS (arm64) and Windows binaries
- Creates a GitHub Release with all assets
- Auto-updates the Homebrew formula with the new sha256

### Cross-compiling for Windows from macOS

The easiest way is to push to GitHub and let the CI build it. For local builds:

```bash
# Option A: cargo cross (requires Docker)
cargo install cross
cross build --release --target x86_64-pc-windows-gnu

# Option B: mingw (may have linker issues with WebView2)
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64
cargo build --release --target x86_64-pc-windows-gnu
```

---

## Tech stack

| | |
|--|--|
| UI framework | [Dioxus 0.6](https://dioxuslabs.com/) — Rust, renders via WebView |
| HTTP client | [reqwest](https://github.com/seanmonstar/reqwest) |
| Async runtime | [Tokio](https://tokio.rs/) |
| JSON | [serde_json](https://github.com/serde-rs/json) |
| File picker | [rfd](https://github.com/PolyMeilex/rfd) |
