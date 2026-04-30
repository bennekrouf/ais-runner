# AIS Runner

A desktop tool for running and testing **Azure Logic Apps Standard** workflows locally, built with [Dioxus](https://dioxuslabs.com/) (Rust).

---

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

## Prerequisites

These must be installed and on PATH on any machine running the app:

| Tool | Purpose | Install |
|------|---------|---------|
| **Node.js** | Runtime for func and azurite | https://nodejs.org |
| **Azure Functions Core Tools** (`func`) | Runs workflows locally via `func start` | `npm install -g azure-functions-core-tools@4` |
| **Azurite** | Local Azure Storage emulator | `npm install -g azurite` |
| **Azure CLI** (`az`) | Fetches Service Bus connection strings, lists subscriptions | https://aka.ms/installazurecli |

The app shows a warning banner on startup for any tool that is not found.

> **Windows only:** WebView2 Runtime is required (pre-installed on Windows 10 21H1+ and Windows 11 via Edge). On older machines download it from https://developer.microsoft.com/microsoft-edge/webview2/

---

## Building from source

### macOS / Linux

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install Dioxus CLI (optional — only needed for dx serve / dx bundle)
cargo install dioxus-cli

# Build
cargo build --release

# Run
./target/release/ais-runner
```

### Windows (native)

```powershell
# Install Rust from https://rustup.rs, then:
cargo build --release
.\target\release\ais-runner.exe
```

---

## Cross-compiling for Windows from macOS

> The recommended approach is **Option A or B** below. Plain `cargo build --target x86_64-pc-windows-gnu` may fail due to Dioxus's WebView2 dependency.

### Option A — GitHub Actions (simplest)

Create `.github/workflows/build-windows.yml`:

```yaml
name: Build Windows

on:
  push:
    branches: [main]
  workflow_dispatch:

jobs:
  build:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Build
        run: cargo build --release
      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: ais-runner-windows
          path: target/release/ais-runner.exe
```

Push to GitHub, let the action run, then download `ais-runner.exe` from the Actions tab.

### Option B — `cargo cross` (local, requires Docker)

```bash
# Install cross
cargo install cross

# Make sure Docker Desktop is running, then:
cross build --release --target x86_64-pc-windows-gnu
```

Output: `target/x86_64-pc-windows-gnu/release/ais-runner.exe`

### Option C — mingw (may have linker issues with WebView2)

```bash
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64
```

Add to `.cargo/config.toml`:

```toml
[target.x86_64-pc-windows-gnu]
linker = "x86_64-w64-mingw32-gcc"
```

```bash
cargo build --release --target x86_64-pc-windows-gnu
```

---

## Distributing to colleagues

The compiled binary is self-contained — no Rust installation needed on the target machine.

Colleagues need only:
1. The `ais-runner.exe` (or `ais-runner` on macOS/Linux)
2. The prerequisites listed above (Node.js, func, azurite, az)
3. Access to the `logic_apps/` folder of the repo

The app stores its configuration (recent folders, Service Bus namespace, subscription ID) in:
- **macOS/Linux:** `~/.config/ais-runner/config.json`
- **Windows:** `%APPDATA%\ais-runner\config.json`

---

## Tech stack

| | |
|--|--|
| UI framework | [Dioxus 0.6](https://dioxuslabs.com/) — Rust, renders via WebView |
| HTTP client | [reqwest](https://github.com/seanmonstar/reqwest) |
| Async runtime | [Tokio](https://tokio.rs/) |
| JSON | [serde_json](https://github.com/serde-rs/json) |
| File picker | [rfd](https://github.com/PolyMeilex/rfd) |
