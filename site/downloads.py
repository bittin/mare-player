#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Fill the site's direct-download table from the latest GitHub release.

Usage: downloads.py INDEX_HTML < release.json

release.json is the output of
`gh release view --json tagName,url,assets`. The table replaces the
`<!-- downloads -->` marker in INDEX_HTML. Assets are matched by name, so
the table lists exactly what the release workflow published; anything it
doesn't recognise (source RPMs, checksums, provenance) is left out. With no
release data (offline, no `gh`), the marker becomes a link to the latest
release page instead.
"""

import html
import json
import re
import sys

REPO = "https://github.com/glima/mare-player"
MARKER = "<!-- downloads -->"

VARIANTS = [("mare-player", "Standalone app"), ("cosmic-applet-mare", "Panel applet")]
ARCHES = ["x86_64", "aarch64"]
DEB_ARCH = {"amd64": "x86_64", "arm64": "aarch64"}

# (row label, file kind, pattern giving the package name and architecture)
FORMATS = [
    ("Debian, Ubuntu", ".deb", re.compile(r"^(?P<pkg>[a-z-]+)_[^_]+_(?P<arch>amd64|arm64)\.deb$")),
    ("Other RPM distributions", ".rpm", re.compile(r"^(?P<pkg>[a-z-]+)-[0-9][^/]*\.(?P<arch>x86_64|aarch64)\.rpm$")),
    ("Any distribution", ".tar.gz", re.compile(r"^(?P<pkg>[a-z-]+)-(?P<arch>x86_64|aarch64)\.tar\.gz$")),
]


def fallback() -> str:
    return (
        '<article class="card install-card downloads">\n'
        "  <h3>Other distributions</h3>\n"
        f'  <p>.deb and .rpm packages and tarballs are attached to every <a href="{REPO}/releases/latest">release</a>.</p>\n'
        "</article>"
    )


def table(release: dict) -> str:
    found = {}
    for asset in release.get("assets", []):
        for label, kind, pattern in FORMATS:
            m = pattern.match(asset["name"])
            if m:
                arch = DEB_ARCH.get(m["arch"], m["arch"])
                found[(kind, m["pkg"], arch)] = asset
    if not found:
        return fallback()

    tag = html.escape(release["tagName"])
    url = html.escape(release["url"])
    head = "".join(f'<th scope="col">{name}</th>' for _, name in VARIANTS)
    rows = []
    for label, kind, _ in FORMATS:
        cells = []
        for pkg, _ in VARIANTS:
            links = []
            for arch in ARCHES:
                asset = found.get((kind, pkg, arch))
                if asset:
                    name = html.escape(asset["name"])
                    href = html.escape(asset["url"])
                    links.append(f'<a href="{href}" title="{name}" aria-label="{name}">{arch}</a>')
            cells.append("<td>" + (" ".join(links) or "—") + "</td>")
        rows.append(f'<tr><th scope="row">{label} <small>{kind}</small></th>{"".join(cells)}</tr>')

    sums = next((a for a in release.get("assets", []) if a["name"] == "SHA256SUMS"), None)
    verify = (
        f'Check downloads against <a href="{html.escape(sums["url"])}">SHA256SUMS</a> and the signed provenance; '
        if sums
        else "Every release ships checksums and signed provenance; "
    )
    return (
        '<article class="card install-card downloads">\n'
        "  <h3>Other distributions</h3>\n"
        f'  <p>Packages and tarballs from the latest release, <a href="{url}">{tag}</a>. '
        "They don't update themselves, so on Fedora prefer COPR.</p>\n"
        '  <div class="table-wrap"><table>\n'
        f'    <thead><tr><th scope="col">Package</th>{head}</tr></thead>\n'
        "    <tbody>\n      " + "\n      ".join(rows) + "\n    </tbody>\n"
        "  </table></div>\n"
        f'  <p class="note">{verify}<a href="{REPO}/blob/main/SECURITY.md#verifying-a-download">see how</a>.</p>\n'
        "</article>"
    )


def main() -> None:
    index = sys.argv[1]
    raw = sys.stdin.read().strip()
    try:
        fragment = table(json.loads(raw)) if raw else fallback()
    except (ValueError, KeyError, TypeError):
        fragment = fallback()
    with open(index, encoding="utf-8") as f:
        page = f.read()
    if MARKER not in page:
        sys.exit(f"{index}: no {MARKER} marker")
    with open(index, "w", encoding="utf-8") as f:
        f.write(page.replace(MARKER, fragment))


if __name__ == "__main__":
    main()
