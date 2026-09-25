# AIS Runner

## Release notes

Every change a user of AIS Runner could notice gets a note in `CHANGELOG.md`, in
the same PR as the change. The notes are published as-is at
<https://mayorana.ch/en/apps/ais-runner/releases> and in the GitHub Release body.

- Add bullets under `## [Unreleased]`, grouped under `### Added`, `### Changed`,
  `### Fixed` or `### Removed`. If there is no `[Unreleased]` heading, create
  it directly above the newest version.
- Never write a version heading yourself. `scripts/release.sh` renames
  `[Unreleased]` to the version and date when it cuts the release.
- Write for someone using the app, not for a reviewer: what they will see or no
  longer run into, and why it matters. No function, module or file names from
  the codebase; the files, tabs and settings the user works with are fine.
- Wrap at 80 columns and match the tone of the existing entries.
- No user-visible change (tests, refactors, CI, docs)? Write no note and put the
  `no-notes` label on the PR. The `Release notes` check fails a PR that
  changes app files without either.
