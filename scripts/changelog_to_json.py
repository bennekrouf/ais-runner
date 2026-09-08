#!/usr/bin/env python3
"""Turn CHANGELOG.md into the release feed the website and the app read.

CHANGELOG.md is the only place a release note is written. This script is what
lets that single copy reach the three places it has to appear:

  * `releases.json`, published next to the builds on mayorana.ch and rendered
    at /en/apps/ais-runner/releases — the public, indexable page.
  * the GitHub Release body, so the tag says what shipped rather than only how
    to download it.
  * anything in-app that wants "what's new in the version you are being offered".

Usage:
    changelog_to_json.py --json  [--version X.Y.Z] [--date YYYY-MM-DD]
    changelog_to_json.py --notes --version X.Y.Z

`--json` writes the whole feed to stdout. `--notes` prints one version's
section as markdown.

`--version`/`--date` together stamp a release whose heading is still
`## [Unreleased]`: at release time the version exists and the date is known,
but the file has not been edited yet, and a release must not be held up by
that. The stamped entry is emitted as that version; the file itself is left
alone.
"""

import argparse
import json
import re
import sys
from datetime import date, datetime, timezone
from pathlib import Path

NAME = "ais-runner"
PRODUCT = "AIS Runner"
PAGE = "https://mayorana.ch/en/apps/ais-runner"

# "## [0.5.49] - 2026-09-05" or "## [Unreleased]"
HEADING = re.compile(r"^##\s+\[(?P<version>[^\]]+)\]\s*(?:-\s*(?P<date>\S+))?\s*$")
SUBHEADING = re.compile(r"^###\s+(?P<heading>.+?)\s*$")
BULLET = re.compile(r"^-\s+(?P<text>.+?)\s*$")
SEMVER = re.compile(r"^\d+\.\d+\.\d+$")
ISO_DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")


def parse(text):
    """Every `## [...]` section, in file order, with its bullets."""
    releases = []
    release = None
    section = None

    for raw in text.splitlines():
        line = raw.rstrip()

        # A horizontal rule at top level closes the last release, so the
        # footer paragraph below it is not read as one of its bullets.
        if line.strip() == "---":
            release = section = None
            continue

        match = HEADING.match(line)
        if match:
            release = {
                "version": match.group("version").strip(),
                "date": (match.group("date") or "").strip(),
                "sections": [],
            }
            releases.append(release)
            section = None
            continue

        if release is None:
            continue

        match = SUBHEADING.match(line)
        if match:
            section = {"heading": match.group("heading"), "items": []}
            release["sections"].append(section)
            continue

        match = BULLET.match(line)
        if match:
            if section is None:  # bullets before any "### " subheading
                section = {"heading": "", "items": []}
                release["sections"].append(section)
            section["items"].append(match.group("text"))
            continue

        # A wrapped bullet: indented continuation of the previous item.
        if section and section["items"] and raw.startswith((" ", "\t")) and line.strip():
            section["items"][-1] += " " + line.strip()

    return releases


def stamp(releases, version, when):
    """Give the release being cut its number and date, wherever it sits.

    Handles both shapes: the heading already names the version (the file was
    edited before tagging), or the notes are still under `[Unreleased]`.
    """
    if not version:
        return
    for release in releases:
        if release["version"] == version:
            release["date"] = release["date"] if ISO_DATE.match(release["date"]) else when
            return
    for release in releases:
        if release["version"].lower() == "unreleased":
            release["version"] = version
            release["date"] = when
            return


def published(releases):
    """Only real, dated versions reach the feed — `[Unreleased]` is a draft."""
    out = []
    for release in releases:
        if SEMVER.match(release["version"]) and ISO_DATE.match(release["date"]):
            out.append(release)
    out.sort(key=lambda r: [int(p) for p in r["version"].split(".")], reverse=True)
    return out


def to_markdown(release):
    parts = []
    for section in release["sections"]:
        if section["heading"]:
            parts.append(f"### {section['heading']}\n")
        parts.extend(f"- {item}" for item in section["items"])
        parts.append("")
    return "\n".join(parts).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--changelog", default="CHANGELOG.md", type=Path)
    parser.add_argument("--json", action="store_true", help="write the full feed to stdout")
    parser.add_argument("--notes", action="store_true", help="write one version's markdown")
    parser.add_argument("--version", help="version being released, without the leading v")
    parser.add_argument("--date", help="release date, YYYY-MM-DD (default: today, UTC)")
    args = parser.parse_args()

    if not args.changelog.exists():
        print(f"{args.changelog}: not found", file=sys.stderr)
        return 1

    when = args.date or date.today().isoformat()
    version = (args.version or "").lstrip("v")

    releases = parse(args.changelog.read_text(encoding="utf-8"))
    stamp(releases, version, when)
    releases = published(releases)

    if args.notes:
        if not version:
            print("--notes needs --version", file=sys.stderr)
            return 2
        match = next((r for r in releases if r["version"] == version), None)
        # Not an error: a release cut without a changelog entry still ships,
        # it just has nothing extra to say.
        print(to_markdown(match) if match else "", end="")
        return 0

    feed = {
        "name": NAME,
        "product": PRODUCT,
        "page": PAGE,
        "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "releases": [
            {
                "version": r["version"],
                "tag": f"v{r['version']}",
                "date": r["date"],
                "sections": r["sections"],
            }
            for r in releases
        ],
    }
    json.dump(feed, sys.stdout, indent=2, ensure_ascii=False)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
