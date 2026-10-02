---
name: ais-runner
description: Help someone use AIS Runner, the desktop app for running and testing Azure Logic Apps Standard workflows locally (Azurite, Service Bus emulator, func, mock APIs, recorded test scenarios, the ais-test CLI). Use this whenever the user mentions AIS Runner or ais-test, or is running Logic Apps Standard locally and hits a func start failure, a workflow that won't trigger or fails only locally, Azurite or Service Bus emulator trouble, local.settings.json / connections.json problems, or wants to write or debug a scenario under .ais-runner/scenarios. Also use it when they want to report an AIS Runner bug or ask the author for help.
---

# AIS Runner

AIS Runner is a desktop app (macOS, Windows, Linux) for developing and testing
Azure Logic Apps Standard workflows on a developer's machine, without deploying
to Azure. It opens a Logic Apps project folder, starts the local emulators and
the Functions host, runs workflows, and records and replays multi-step test
scenarios.

The person you are helping is a developer working on their own (often a
client's) Logic Apps project. They usually have the app open next to you and
the project checked out. When you can read the project, do: the answer to most
questions is in `workflow.json`, `connections.json` and `local.settings.json`,
not in general Logic Apps knowledge.

## How the app fits together

Keep this model in mind; most problems are one of these pieces being down,
started in the wrong order, or pointed at Azure instead of at the local stack.

- **Azurite**: local Azure Storage. The Logic Apps runtime keeps its own state
  there (`AzureWebJobsStorage`), so without it nothing runs at all.
- **SB Emulator**: the Azure Service Bus emulator, run in Docker. Needs Docker
  Desktop (or the Docker engine on Linux) to be running.
- **Mock APIs**: scans the workflows for outbound HTTP calls, serves stub
  responses locally, and points URL app settings at them.
- **▶ func**: starts the Logic Apps runtime (`func start`). Before starting it
  the app fixes what it can: creates a missing `package.json`, fixes ARM syntax
  in `connections.json`, switches managed-identity connections to local
  connection strings, fills empty settings it has a local value for, and points
  settings that still aim at Azure at the local emulators. It restores
  `connections.json` and `local.settings.json` when func stops.
- **☕ Java**: `mvn package` then `mvn azure-functions:run`, for Java function
  apps.

**The runtime reads `local.settings.json` and `connections.json` only when func
starts.** Any change to settings, connections or the mock while func is
running has no effect until func is restarted. This one fact explains a large
share of "I changed it and nothing happened".

The safe start order is **Azurite → SB Emulator (if the project uses Service
Bus) → Mock APIs (if it calls HTTP APIs) → ▶ func**. Started before Azurite or
the Service Bus emulator, func attaches no queue listeners; started before the
mock, it never sees the mock's URLs.

## Main screens

- **Workflows** (left list): every workflow under the project, with health and
  trigger type. Selecting one shows an analysis bar (trigger, queues read and
  written, blob containers, HTTP calls, Liquid maps) and tabs:
  **Source** (`workflow.json`), **Run** (trigger it with a generated payload and
  watch each action live), **Logs** (app logs for that workflow only).
- **Connections**: Service Bus queues with message and dead-letter counts and a
  way to send test messages; SQL and SFTP connections; Cosmos DB endpoint tests;
  Blob containers to browse, upload and delete; Liquid/XSLT **Maps** with a
  tester.
- **Tests**: record (**● Create scenario**), edit and replay scenarios
  (**▶ Run**, **▶▶ Run all**, **▶▶ Run group**). See
  `references/scenarios.md` for the file format and the `ais-test` CLI.
- **DevOps**: Azure DevOps build pipelines and a grid of which build is deployed
  to which environment, via the `az pipelines` CLI.
- **Logs** (bottom): **Console** (func or Java output, noise filtered),
  **Azurite**, **Service Bus**.
- **⚠ N unset** chip in the toolbar: settings AIS Runner has no local value for.
  Hovering shows which workflows can't run until they are set; clicking opens
  Settings.

Config lives in `~/.config/ais-runner/config.json` (macOS, Linux) or
`%APPDATA%\ais-runner\config.json` (Windows). Release notes for every version:
<https://mayorana.ch/en/apps/ais-runner/releases>.

## When something fails

1. **Get the actual message.** Ask for the exact text from the Console log or
   the failed step, not a paraphrase. AIS Runner already explains many
   failures in its own words (lines starting with ⚠ or ❌); those are usually
   right about the cause.
2. **Check what is running** against the start order above. A large share of
   failures are a service that is down, or func started before it.
3. **Look up the symptom** in `references/troubleshooting.md`, which lists the
   known failure modes with their causes and fixes. Read it before guessing:
   several local-only failures look like workflow bugs and are not.
4. **Read the project files** that the symptom points at, and fix the cause in
   the project when it lives there (a missing connection, a setting with no
   local value, a scenario step).
5. If none of that explains it, it may be an AIS Runner bug. Offer to draft a
   report (below).

Separate what the app does from what Azure does. AIS Runner runs everything
locally on purpose; it blocks a run when a connection still points at Azure
rather than letting it hit the cloud. Don't suggest "just use the real Azure
endpoint" as a fix.

## Version and logs

- The app version is in the window title (`AIS Runner 0.5.59`). Ask for it
  early: the user may be on an old version where the problem is already fixed,
  and the release notes say which version fixed what.
- `crash.log` and `tool-check.log` (which tools the app found on the machine)
  are in the app's data folder:
  - macOS: `~/Library/Application Support/AIS Runner/`
  - Windows: `%LOCALAPPDATA%\AIS Runner\`
  - Linux: `~/.local/share/AIS Runner/`
- Azurite's own log is shown in the **Azurite** log tab.

## Reporting a problem to the author

When the problem looks like an AIS Runner bug, or the user wants to send
feedback, help them write a report they can send. The user sends it, not you:
never create an issue, send an email or submit a form on their behalf.

Their logs and files come from client projects, so they can contain client
names, internal hostnames, connection strings, keys and message contents.
Before showing the draft, replace anything like that with neutral placeholders
(`<client>`, `<host>`, `<queue>`), then tell the user what you replaced and ask
them to check the rest. Keep only the lines of log that show the failure, not
whole files.

Use this structure:

~~~markdown
**AIS Runner version:** 0.5.59
**OS:** macOS 15.1 (Apple Silicon) / Windows 11 / Ubuntu 24.04

**What I did**
1. …

**What I expected**
…

**What happened instead**
…

**Relevant log lines** (Console, Azurite or Service Bus tab)
```
…
```

**Workaround found, if any**
…
~~~

Then give them the two ways to send it:

- A GitHub issue at <https://github.com/bennekrouf/ais-runner/issues/new>:
  they paste the title and body and submit it themselves. Issues there are
  public, which is one more reason the draft must be scrubbed.
- The contact form at <https://mayorana.ch/en/contact>, for anything they
  would rather not post publicly.
