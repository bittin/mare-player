#!/usr/bin/env python3
"""Generate one changelog range from the previous published stable release."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]


def version(tag):
    match = re.fullmatch(r"v(\d+)\.(\d+)\.(\d+)", tag)
    return tuple(map(int, match.groups())) if match else None


def previous_release(releases, tag, is_ancestor):
    current = version(tag)
    if current is None:
        raise ValueError("Expected a stable vX.Y.Z release tag")
    # Publication dates change on reruns/backfills. Prefer the highest lower
    # stable version on this tag's history, not merely the nearest Git tag.
    candidates = {
        release["tag_name"] for release in releases
        if not release["draft"] and not release["prerelease"] and release["published_at"]
        and version(release["tag_name"]) is not None
        and version(release["tag_name"]) < current
    }
    return next((candidate for candidate in sorted(candidates, key=version, reverse=True)
                 if is_ancestor(candidate)), None)


def published_releases(repository):
    # An API/authentication failure must stop generation, not silently fall
    # back to Git tags and omit changes from a failed release again.
    pages = json.loads(subprocess.check_output(
        ["gh", "api", f"repos/{repository}/releases?per_page=100", "--paginate", "--slurp"],
        text=True,
    ))
    return [release for page in pages for release in page]


def generate_notes(repository, tag, releases, directory=ROOT):
    shallow = subprocess.check_output(
        ["git", "rev-parse", "--is-shallow-repository"], cwd=directory, text=True,
    ).strip()
    if shallow == "true":
        raise ValueError("Release notes require full Git history; fetch with --unshallow --tags first")

    def commit(ref):
        return subprocess.check_output(
            ["git", "rev-parse", "--verify", "--end-of-options", f"{ref}^{{commit}}"],
            cwd=directory, text=True,
        ).strip()

    current = commit(tag)

    def is_ancestor(candidate):
        result = subprocess.run(
            ["git", "merge-base", "--is-ancestor", commit(candidate), current], cwd=directory,
        )
        if result.returncode not in (0, 1):
            result.check_returncode()
        return result.returncode == 0

    previous = previous_release(releases, tag, is_ancestor)
    commit_range = f"{previous}..{tag}" if previous else current
    notes = subprocess.check_output(
        ["git-cliff", "--offline", "--strip", "header", "--tag-pattern", f"^{re.escape(tag)}$",
         "--", commit_range], cwd=directory, text=True,
    ).strip()
    # Filtering tag boundaries makes unreleased intermediate tags part of this
    # single release, rather than splitting them into repeated section headings.
    if previous:
        notes += f"\n\n**Full Changelog**: https://github.com/{repository}/compare/{previous}...{tag}"
    return notes + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True, help="GitHub OWNER/REPO")
    parser.add_argument("--tag", required=True, help="Published or upcoming vX.Y.Z tag")
    args = parser.parse_args()
    if version(args.tag) is None:
        parser.error("--tag must be a stable vX.Y.Z tag")
    if not re.fullmatch(r"[\w.-]+/[\w.-]+", args.repository):
        parser.error("--repository must be OWNER/REPO")
    try:
        notes = generate_notes(args.repository, args.tag, published_releases(args.repository))
    except (subprocess.CalledProcessError, ValueError, OSError) as error:
        print(f"Release-note generation failed: {error}", file=sys.stderr)
        return 1
    print(notes, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
