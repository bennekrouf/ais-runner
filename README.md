# AIS Runner

A desktop tool for developing and testing **Azure Logic Apps Standard** workflows locally — without pushing to Azure.

Built with [Dioxus](https://dioxuslabs.com/) (Rust) · macOS · Windows · Linux

[![Release](https://img.shields.io/github/v/release/Bennekrouf/ais-runner?label=latest)](https://github.com/Bennekrouf/ais-runner/releases/latest)

---

## Install

Builds are **free for individuals** and download from [mayorana.ch](https://mayorana.ch/en/apps). Each release on GitHub carries a
`latest.json` with the `sha256` of every artifact, so you can verify what you
downloaded.

### macOS (Apple Silicon)

Download [`ais-runner-macos-arm64.dmg`](https://mayorana.ch/downloads/ais-runner/latest/ais-runner-macos-arm64.dmg), open it, and drag **AIS Runner** to Applications.
Signed with Apple Developer ID and notarized — opens with a normal double-click.

### Windows

Download [`ais-runner-setup.exe`](https://mayorana.ch/downloads/ais-runner/latest/ais-runner-setup.exe) and run it.  
The wizard installs the app and optionally installs all runtime dependencies in one step.

### Linux (x86\_64)

```bash
curl -L https://mayorana.ch/downloads/ais-runner/latest/ais-runner-linux-x86_64.tar.gz | tar xz
cd ais-runner-linux-x86_64
sudo ./setup-linux.sh && ./ais-runner
```

`setup-linux.sh` detects your distro (Debian/Ubuntu, Fedora, Arch) and installs WebKitGTK, Node.js 20, Azure CLI, Azurite, and Azure Functions Core Tools.

---

## What it does

### Workflows

- Browse all workflows in your `logic_apps/` folder, with health status and trigger type chips
- Filter by name, navigate with ↑ / ↓ arrow keys
- **Analysis bar** — see trigger type, Service Bus queues read/written, blob containers, HTTP calls, and Liquid maps
- **Source tab** — view `workflow.json`, copy to clipboard or open in VS Code / system editor
- **Run tab** — trigger any workflow with an auto-generated JSON payload; live action timeline updates in real time
- **Logs tab** — filter app logs to only the selected workflow

### Services (toolbar)

| Button | What it does |
|--------|-------------|
| **Azurite** | Start / stop / reset the local storage emulator |
| **SB Emulator** | Start / stop / reset the Azure Service Bus Docker emulator |
| **Mock APIs** | Scan the workspace for outbound HTTP calls, serve stubbed responses locally, and point URL app settings at them |
| **▶ func** | `func start` for Node.js Logic Apps (with pre-flight fixes: package.json, connections.json ARM syntax, stub missing settings) |
| **☕ Java** | `mvn package -DskipTests` then `mvn azure-functions:run` for Java function apps |

### Mock APIs

Starting it scans every `workflow.json`, builds a contract of the outbound HTTP
calls (using adjacent `Parse_JSON` schemas to generate example responses), serves
them on a local port, and rewrites the URL-shaped values in
`local.settings.json` to point there. Stopping restores the file.

Traffic appears in the Console prefixed `🎭`. A call the scan never saw comes
back `404 (no match)` — that's the signal to add a fixture or re-scan.

**Start it before func.** Logic Apps reads `local.settings.json` once at host
startup, so a mock started afterwards is invisible to a running host.

Two limits worth knowing:

- Only **app settings** are rewritten. A workflow whose base URL arrives in the
  message payload, or is built from `variables(...)`, is never redirected — use
  a `run_process` stub for those (below).
- When a setting doubles as the AAD `audience` (`"audience": "@{parameters('Jde_Url')}"`),
  pointing it at localhost also changes what token is requested, and the call
  fails at auth before reaching the mock.

### Connections tab

- **Service Bus** — list queues with message counts, dead-letter counts, and send test messages
- **SQL** — list detected SQL connections
- **Cosmos DB** — test Cosmos endpoints (emulator or Azure)
- **Blob** — browse containers, upload/delete blobs
- **Maps** — list `.liquid` / `.xslt` templates, see which workflows use each, test with auto-suggested input, choose DotLiquid or Liquid engine
- **SFTP** — list detected SFTP connections

### Tests tab

Record and replay multi-step scenarios against the local emulators —
integration tests without leaving the app:

- **● Create scenario** captures every successful action you take (create a
  container, send a message, trigger a workflow, …) as a step; review, edit,
  or drop any of them before saving
- **▶ Run** replays a saved scenario, step by step, with a live pass/fail per
  step — including waiting for a triggered workflow's run to finish and
  asserting on its outcome
- **▶▶ Run all** / **▶▶ Run group** replay every scenario, or every scenario in
  a subfolder — drop scenarios into a subfolder under `.ais-runner/scenarios/`
  and it groups (and collapses) in the UI automatically
- Scenarios are plain JSON, committed alongside the workflows they test — see
  [Scenarios](#scenarios) below for the file format and the advanced
  `run_process` step

### DevOps tab

Connects to Azure DevOps via the `az pipelines` CLI:

- Left panel: build pipelines grouped by folder
- Right panel: **unified grid** — rows = build runs, columns = environments from the linked release definition
  - 🟢 green = currently deployed in that environment
  - `＋` = click to create a new release
  - `—` = superseded, click to re-deploy (rollback)
- **▶ Build** — pick any branch from the repo and queue a new build
- **Deployed only** filter to focus on what's live

### Logs

| Tab | Content |
|-----|---------|
| **Console** | func / Java output; SB noise, Maven progress, .NET stack frames filtered out |
| **Azurite** | Azurite debug.log; 60 s poll cycles collapsed into a `📡 Polling: queue.name` banner |
| **Service Bus** | SB Emulator Docker output with Windows Fabric noise filtered |

---

## Usage

1. Launch `ais-runner`
2. Select your platform folder (first open) or pick from recents
3. Start **Azurite** then **▶ func** from the toolbar
4. Click any workflow on the left to view, run, and inspect it

Config is stored in `~/.config/ais-runner/config.json` (macOS/Linux) or `%APPDATA%\ais-runner\config.json` (Windows).

---

## Scenarios

A scenario is one JSON file — `.ais-runner/scenarios/<name>.json`, or
`.ais-runner/scenarios/<group>/<name>.json` to group it — describing a
sequence of steps: set up state (create a container, drain a queue), act
(send a message, trigger a workflow), then assert (wait for a run to
succeed, check a message landed, check an action's inputs). The Tests tab
records, edits, and replays them; see [Tests tab](#tests-tab) above.

One advanced step is worth knowing about directly: `run_process` starts a
helper process for the duration of the run — typically a stub server
standing in for an API the mock can't intercept.

```json
{ "action": "run_process",
  "command": "python3",
  "args": [".ais-runner/fixtures/my-stub.py", "8899"],
  "wait_for_port": 8899 }
```

- `wait_for_port` blocks until something is listening, so the next step doesn't
  race the stub's startup. If the process exits before binding, the step fails
  immediately with its exit status rather than waiting out the timeout.
- Relative `workdir` paths resolve against the project root (default), so a
  scenario replays in any checkout.
- The process is killed when the scenario ends — pass **or** fail. Set
  `"stop_at_end": false` to opt out.

The command runs as-is, with no approval prompt. That matches the rest of the
tool: **▶ func** and **☕ Java** already execute whatever code the opened
workspace contains, and both are a single click. What a `run_process` step does
is at least stated in plain sight — in a reviewable JSON file, and echoed into
the run log as `started '<command>'`.

## Service Bus Emulator — known pitfalls

`Config.json` is generated for you, and a hand-edited or otherwise stale copy
(wrong namespace name, `LoggingConfig` instead of `Logging`) is detected and
regenerated automatically — you shouldn't need to touch this file. Two things
that aren't config bugs and can't be auto-fixed:

- **"Ready" ≠ "AMQP broker ready."** Port `:5672` opens when the container
  starts, but the broker needs SQL Edge to finish initialising 10–30 s longer.
  Wait for **"Service Bus emulator ready"** in the console.
- **SB polling triggers fire on a 1-minute recurrence.** ais-runner waits up
  to 75 s for one — don't restart before that window expires.

MSI connections (`AzureBlob`, `ServiceBus`, …) are also patched to local
connection-string auth automatically on every **▶ func** start.

---

## Contributing

### Prerequisites

| Tool | Install |
|------|---------|
| **Rust** | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| **MSYS2 + MinGW-w64** (Windows) | `scripts\setup-windows-dev.ps1` |

### Build from source

```bash
git clone https://github.com/Bennekrouf/ais-runner.git
cd ais-runner
cargo build --release
./target/release/ais-runner   # macOS / Linux
```

Windows — after running `setup-windows-dev.ps1` once:

```powershell
cargo build --release
Copy-Item (Get-ChildItem target\release\build\webview2-com-sys-*\out\x64\WebView2Loader.dll | Select -First 1) target\release\
.\target\release\ais-runner.exe
```

### Project layout

```
src/
  components/   UI — workflow list, run detail, log panel, DevOps, Connections…
  handlers/     Event handlers — func start, azurite, Java, workflow run…
  screens/      Top-level screens — welcome, main
  services/     Azure CLI wrappers, config, workflow parsing, analysis…
crates/
  ais-chain/    Workflow dependency graph (inlined local crate)
scripts/
  release.sh          Cut a release (bump version, tag, push → triggers CI)
  setup-linux.sh      Linux runtime dependency installer (Debian/Fedora/Arch)
  setup-windows.ps1   Windows runtime dependency installer
installer/
  installer.iss       Inno Setup script → ais-runner-setup.exe
.github/workflows/
  release.yml         Build, sign, notarize, and publish all-platform release
  build-mac.yml       CI on push to main
  build-windows.yml
```

### Releasing

```bash
./scripts/release.sh            # auto-bump patch, confirm, push
./scripts/release.sh --minor    # bump minor
./scripts/release.sh 1.0.0      # explicit version
./scripts/release.sh --dry-run  # preview only
```

Pushing a `v*` tag triggers CI which builds all platforms (macOS signed + notarized DMG, Windows installer, Linux tarball), publishes them to mayorana.ch, and creates a GitHub Release carrying the release notes and `latest.json` (the update manifest and checksums). The binaries are not attached to the GitHub Release.

---

## Tech stack

| | |
|--|--|
| UI framework | [Dioxus 0.6](https://dioxuslabs.com/) — Rust, renders via WebView |
| Async runtime | [Tokio](https://tokio.rs/) |
| HTTP client | [reqwest](https://github.com/seanmonstar/reqwest) |
| JSON | [serde / serde_json](https://serde.rs/) |
| Liquid templates | [liquid 0.26](https://github.com/cobalt-org/liquid-rust) |
| AMQP | [fe2o3-amqp](https://github.com/minghuaw/fe2o3-amqp) |
| File picker | [rfd](https://github.com/PolyMeilex/rfd) |
| Clipboard | [arboard](https://github.com/1Password/arboard) |

---

## Licence

Source-available under the [PolyForm Noncommercial License 1.0.0](LICENSE).

- **Free** for personal use, learning, research and hobby projects, and for
  charities, schools, universities and government institutions.
- **Commercial use requires a licence** — including a solo consultant using it
  on client work, and an employee using it at their job.
  [Get in touch](https://mayorana.ch/en/contact).

This is deliberately not an OSI-approved open source licence: the source is
public and readable, but companies using it for work buy a licence.

The name, logo and icons are trademarks and are not covered by that licence —
fork it and rebrand it. See [TRADEMARK.md](TRADEMARK.md).

All 712 third-party dependencies are permissively licensed (MIT, Apache-2.0,
MPL-2.0, Zlib, Unicode-3.0); none are GPL/AGPL.
