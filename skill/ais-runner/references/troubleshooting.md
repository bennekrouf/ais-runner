# AIS Runner troubleshooting

Known failure modes, grouped by where they show up. Each entry: what the user
sees, why it happens, what to do. Match on the user's actual message where you
can; the quoted texts are what the app or the Logic Apps runtime print.

## Contents

- [func won't start, or starts with nothing running](#func-wont-start-or-starts-with-nothing-running)
- [A workflow fails only locally](#a-workflow-fails-only-locally)
- [Service Bus](#service-bus)
- [Mock APIs](#mock-apis)
- [Scenarios (Tests tab and ais-test)](#scenarios-tests-tab-and-ais-test)
- [Install and launch](#install-and-launch)
- [Files changed after a session](#files-changed-after-a-session)

## func won't start, or starts with nothing running

**"Missing value for AzureWebJobsStorage in local.settings.json"**, or
**"⚠ AzureWebJobsStorage points to a remote Azure account — func cannot start
locally."**
The runtime keeps its state in storage. Locally that must be Azurite:
`"AzureWebJobsStorage": "UseDevelopmentStorage=true"`. AIS Runner sets it when
func starts; if the message persists, check `local.settings.json` isn't being
rewritten by something else (a script, an editor extension), then start Azurite
and restart func.

**"⚠ N of M workflow(s) are registered but have no runtime state in Azurite —
they cannot run."**
func started without Azurite, or Azurite was reset or wiped while func ran.
Start Azurite (or Stop then Start it), then restart func.

**"⚠ Azurite port(s) … not responding — click Stop then Start on Azurite to
restart it."**
Azurite crashed or another process holds its ports (10000–10002). Stop and
start it from the toolbar. If the ports are taken by a leftover Azurite from
another tool, stop that one.

**"⚠ '<workflow>' not registered by func — a connection likely failed to
initialise."**
A connection in `connections.json` references a setting that is missing or
points nowhere reachable, and the runtime silently dropped the workflow. Open
**Connections**, find the connection the workflow uses, check its endpoint, fix
`local.settings.json`, restart func.

**"'<workflow>' unhealthy offline — uses managed API connection(s): …"**
Managed API connections (Office 365, Teams, SharePoint and other
`managedApiConnections`) have no local emulator. Those workflows can't run
locally; that is expected, not a bug. Test the rest, or stub the call.

**"⚠ N unset" chip in the toolbar** (0.5.57 and later)
Settings AIS Runner has no local value for. Workflows using them won't run
until they are set: click the chip to open Settings and fill them from the
values in Azure, or dismiss it if those workflows aren't needed today.

## A workflow fails only locally

**"The required OAuth authentication property 'tenant' is missing."**
The HTTP action authenticates with `ActiveDirectoryOAuth`, and its
tenant/clientId/secret parameters come from app settings that only exist in
Azure. AIS Runner does not edit `workflow.json` (it did up to 0.5.53; 0.5.54
removed that). Options: point the call at the mock or a stub so it doesn't
need a token, or give `local.settings.json` real values for those parameters
if the developer is allowed to have them locally. Never put a production
secret in a file that gets committed.

**A run fails with "An action failed. No dependent actions succeeded."**
That is the scope reporting a failure somewhere inside it, not the cause. The
real error is usually on one action inside a `Foreach` or `Until`, or in the
response of a child workflow it called. Open the run in the **Run** tab and
expand down to the failing action; scenario failures already name it.

**The run is blocked before it starts, naming connections that point at
Azure.**
AIS Runner refuses to run a workflow whose connections still resolve to the
cloud (`*.servicebus.windows.net`, `*.database.windows.net`, a non-local https
endpoint). Starting func fixes the ones it can map to a local emulator or the
mock. The rest are listed with what to set; set them and restart func.

**A setting was changed, but the run still uses the old value.**
The runtime reads `local.settings.json` only at startup. Restart func.

## Service Bus

**"SB Emulator is not running — start it from the toolbar first."** /
**"hint: make sure Docker Desktop is running."** /
**"Docker Desktop is installed but not running"**
The emulator runs in Docker. Start Docker Desktop (or the Docker service on
Linux), then **SB Emulator**.

**Messages sent, but nothing happens for a while.**
Two normal delays. The emulator reports "ready" when its port opens, but the
broker needs another 10–30 s for SQL Edge to initialise: wait for **"Service
Bus emulator ready"** in the Console. And Service Bus–triggered workflows poll
on a 1-minute recurrence; AIS Runner waits up to 75 s for one. Restarting
before that only resets the wait.

**Messages stay in the trigger queue and no run appears.**
func was started before the emulator, so it attached no listeners, or the
runtime stopped consuming the queue. Restart func with the emulator already
running. From 0.5.59 a scenario's `wait_for_run` reports "N message(s) waiting
unconsumed in '<queue>'" and restarts func once by itself.

**A queue the workflow uses doesn't exist in the emulator.**
The emulator's `Config.json` is generated from the queues the workflows use;
restart the emulator after adding a queue. Don't hand-edit `Config.json`: a
stale or hand-edited one is detected and regenerated.

## Mock APIs

**A call returns "404 (no match)" from the mock.**
The mock's scan never saw that call. Re-scan (stop and start Mock APIs) or add
a fixture.

**The workflow still calls the real API.**
Either the mock was started after func (restart func), or the base URL doesn't
come from an app setting: the mock only rewrites URL-shaped **app settings**.
A URL that arrives in the message payload or is built with `variables(...)` is
never redirected; use a `run_process` stub in a scenario instead (see
`scenarios.md`).

**The call fails at authentication even with the mock running.**
The setting the mock rewrote is also used as the AAD `audience`
(`"audience": "@{parameters('Erp_Url')}"`), so pointing it at localhost changes
the token that's requested. Use a stub on a different setting, or remove the
dependency on the token for the local run.

## Scenarios (Tests tab and ais-test)

**"Logic Apps runtime (func) is not reachable"**
func isn't running. From 0.5.55 the Tests view starts func by itself when it's
the only service missing; it won't when Azurite or the Service Bus emulator is
also down, because func started before them attaches no listeners. Start those
first. `ais-test` never starts anything: the emulators and func must already be
up.

**`wait_for_run` times out with "no terminal run yet".**
The workflow never ran or is still running. Check the trigger actually fired
(right queue, right message shape), the Service Bus polling delay above, and
`timeout_ms` (default 30 s, often too short for Service Bus–triggered flows).

**`wait_for_message` never sees a message that the workflow did send.**
Another workflow consumes that queue and peek-locks each message within a
second. Assert with `expect_action` on the action that sent it instead: run
history is durable, the queue isn't.

**An edited scenario still runs the old version** (before 0.5.58).
Reload the Tests list, or update: from 0.5.58 every run reads the file from
disk.

**`set_settings` has no effect.**
Follow it with `restart_func`; the runtime only reads settings at startup.

## Install and launch

**Linux: "version GLIBC_2.39 not found" at launch.**
Builds before 0.5.56 needed Ubuntu 24.04 or newer. Update; current builds run
on Ubuntu 22.04, Debian 12 and newer.

**Linux: the window doesn't open or is blank.**
`libwebkit2gtk-4.1` is missing. Run `setup-linux.sh` from the download, which
also installs Node.js, Azure CLI, Azurite and Functions Core Tools.

**Tools not found when the app is started from the Dock, Finder or Start
menu** (`func`, `node`, `az`, `mvn`).
The app adds the usual install locations to its PATH, but tools installed
somewhere unusual can still be missed. `tool-check.log` in the app's data
folder lists what it found. Install the tool normally, or start the app from a
terminal where the tool works.

**Ports still held after quitting** (Azurite, func, the emulator).
Fixed in 0.5.36 (processes are stopped on exit) and 0.5.49 (also on Ctrl-C,
`kill` or logout). On older versions, stop the leftover processes or update.

## Files changed after a session

AIS Runner temporarily patches `connections.json` and `local.settings.json`
while func runs and restores them on stop. If the app was killed hard (power
loss, force quit before 0.5.49), they can stay patched: `git status` shows them
changed. Opening the project again restores them. Workflows edited by versions
up to 0.5.53 (OAuth blocks stripped) are put back the same way.
