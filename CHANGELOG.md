# Changelog

What changed in each release of **AIS Runner**, the desktop app for developing
and testing Azure Logic Apps Standard workflows locally.

The public version of this page — with the download for each release — lives at
<https://mayorana.ch/en/apps/ais-runner/releases>. It is generated from this
file by `scripts/changelog_to_json.py`, so this file is the only place a
release note is written.

Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning: [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Each heading is dated on the day its tag was pushed. Releases that carried only
build or packaging work say so rather than being hidden: the version numbers a
user sees in the update prompt should all be accounted for.

## [Unreleased]

### Changed

- Sample projects and screenshots now use a neutral placeholder company name
  instead of a real customer's.
- A pre-commit hook keeps customer names and internal hostnames out of commits.

## [0.5.49] - 2026-09-05

### Added

- Ctrl-C, `kill`, a logout or a cancelled CI step now run the same teardown as
  quitting the app: `connections.json` and `workflow.json` are restored to the
  committed versions, `local.settings.json` is returned to its pre-scenario
  state, and every emulator or `func` process the app started is stopped.
  Previously a signal took the process out between two instructions and left
  patched files patched and Azurite reparented to init.

## [0.5.48] - 2026-09-05

### Added

- Opening a project heals patches left behind by a session that did not exit
  cleanly. `connections.json` and `workflow.json` files still carrying local
  run patches are restored from their snapshots instead of staying dirty in the
  working tree indefinitely.

### Changed

- A failed run now explains itself one level down: instead of the runtime's
  "An action failed. No dependent actions succeeded.", the panel walks into the
  `Foreach` repetition and the invoked workflow's response body to report the
  action that actually broke.

## [0.5.47] - 2026-09-03

### Changed

- Release pipeline only — no user-visible change.

## [0.5.46] - 2026-09-03

### Fixed

- Window titles no longer carry a "Local" prefix, so two AIS windows are
  distinguishable in the macOS window list.

## [0.5.45] - 2026-09-03

### Added

- Overlap analysis for Event Grid subscriptions. Two subscriptions on the same
  system topic whose filters overlap deliver the same event twice, and nothing
  in the Azure portal says so — the Event Grid panel now flags the overlapping
  pairs, along with subscriptions that have no dead-letter destination and so
  discard events once the retry policy is exhausted.

## [0.5.44] - 2026-09-03

### Added

- Event Grid subscriptions are grouped by topic in the panel, with the
  overlapping ones drawn together rather than scattered down a flat list.

## [0.5.43] - 2026-09-03

### Added

- JDK selection for Java function apps. AIS Runner detects the JDKs installed
  on the machine and lets you pick which one `mvn package` and
  `mvn azure-functions:run` use, instead of inheriting whatever `JAVA_HOME`
  happens to hold.
- Workflow authentication patching for local runs. HTTP actions that
  authenticate with `ActiveDirectoryOAuth` cannot resolve their tenant, client
  id and secret locally — the parameters come from app settings that only exist
  in Azure — so the action failed before it ever reached a stub. The auth block
  is now stripped for the local run and restored when `func` stops.

## [0.5.42] - 2026-09-02

### Added

- `connections.json` is snapshotted before a local run and restored when `func`
  stops. The Logic Apps runtime reads that file and only that file, so the
  patched version has to land there — but it no longer *stays* there, and the
  working tree is dirty only while `func` is running.

## [0.5.41] - 2026-09-02

### Changed

- The update check sends a versioned User-Agent, so per-version adoption is
  visible in the download logs — the number that says how many people are still
  on a build with a bug that is already fixed.

## [0.5.40] - 2026-09-02

### Added

- Ports held by a leftover process are reclaimed automatically. Starting
  Azurite or `func` after a previous session was killed no longer fails with a
  bind error that names no owner.

## [0.5.39] - 2026-09-02

### Added

- Port conflicts name the process holding the port, instead of reporting
  "address already in use" and leaving you to find it.

## [0.5.38] - 2026-09-02

### Added

- KPI chips on scenario groups: pass rate and verdict on the group header, with
  a running scenario held out of the count until it settles.

## [0.5.37] - 2026-09-02

### Changed

- Packaging only — no user-visible change.

## [0.5.36] - 2026-09-01

### Fixed

- Every process the app spawned — Azurite, the Service Bus emulator, `func`,
  `mvn` — is terminated when the app exits. They used to survive it and hold
  their ports.

## [0.5.35] - 2026-09-01

### Fixed

- macOS notifications are initialised at startup, so the first notification no
  longer raises the system's "Where is use_default?" dialog.

## [0.5.34] - 2026-08-31

### Fixed

- Installer permissions on Windows, and port handling for scenario runs.

## [0.5.33] - 2026-08-30

### Added

- A scenario run or a full sweep can be cancelled. Cancellation is cooperative:
  the step in flight finishes, nothing further starts, and the teardown still
  runs.

## [0.5.32] - 2026-08-30

### Added

- `function_apps/local.settings.json` is seeded with the keys a Java function
  app needs, so `mvn azure-functions:run` starts on a fresh clone instead of
  failing on a missing setting.

## [0.5.31] - 2026-08-29

### Changed

- Internal: the Azure integration was split into `azure::auth` and
  `azure::servicebus`, and moved under a single `azure` module. No behaviour
  change.

---

Releases before 0.5.31 predate this file. Their tags remain on
[GitHub](https://github.com/Bennekrouf/ais-runner/releases).
