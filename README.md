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

## Service Bus Emulator — known pitfalls

The Azure Service Bus Emulator runs via Docker Compose. ais-runner generates `Config.json`
automatically, but if you ever edit it by hand or use the emulator outside ais-runner, keep
these rules in mind:

### 1. Namespace name must be exactly `sbemulatorns`

```json
{ "Name": "sbemulatorns" }   ✅
{ "Name": "my-namespace" }   ❌  NullReferenceException on startup
```

### 2. Logging key is `Logging`, not `LoggingConfig`

```json
{ "Logging": { "Type": "Console" } }     ✅
{ "LoggingConfig": { "Type": "console" } }  ❌  "Logging config cannot be null"
```

### 3. Do not include an empty `Topics` array

Omit `Topics` entirely, or the emulator throws a `NullReferenceException` on startup.

### 4. "Ready" ≠ "AMQP broker ready"

The port `:5672` opens as soon as the container network stack starts, but the AMQP
broker needs SQL Edge to finish initialising first (10–30 s longer). Wait for the
**"Service Bus emulator ready"** message in ais-runner before sending messages — it
probes the actual AMQP handshake, not just the TCP port.

### 5. SB polling triggers fire on a 1-minute recurrence

If your workflow uses `receiveQueueMessages` or `onNewMessagesFromQueueSession`, it
polls every minute by default. ais-runner waits up to 75 s for a run to appear after
you send a test message — do not restart before that window expires.

### 6. MSI connections do not work locally

The Azure IMDS endpoint (`169.254.169.254`) does not exist on a developer machine.
Click **⚙ Setup** in ais-runner to patch `connections.json` — it switches all
`AzureBlob` connections from `ManagedServiceIdentity` to `connectionString`
(pointing to `AzureWebJobsStorage = UseDevelopmentStorage=true`).

Using a **separate connection string per blob connection** causes a
`ListenerFactoryContext` DI conflict when two blob triggers share the same underlying
storage account. All local blob connections must share the single `AzureWebJobsStorage` key.

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
