#!/usr/bin/env python3
"""Create a disposable Cargo cache view with a tidlers packaging workaround.

The pinned tidlers Git source includes .direnv symlinks into its author's Nix
store. cargo vendor follows these and fails on any other machine. Exclude that
non-source directory in a private copy of the checkout, leaving the actual code,
Cargo.lock and the developer's cache unchanged. Cargo computes the vendor
checksums AFTER this metadata-only change. Remove when tidlers excludes .direnv.
"""

import json
from pathlib import Path
import shutil
import sys
import tomllib


def prepare_cache(original: Path, destination: Path, metadata: dict) -> None:
    original = original.resolve()
    destination.mkdir(parents=True)
    # These caches are read-only for the subsequent `cargo vendor --frozen`.
    (destination / "registry").symlink_to(original / "registry", target_is_directory=True)
    (destination / "git").mkdir()
    (destination / "git/db").symlink_to(original / "git/db", target_is_directory=True)
    checkouts = destination / "git/checkouts"
    checkouts.mkdir()

    tidlers = next(p for p in metadata["packages"] if p["name"] == "tidlers")
    manifest = Path(tidlers["manifest_path"]).resolve()
    relative = manifest.relative_to(original / "git/checkouts")
    for checkout in (original / "git/checkouts").iterdir():
        if checkout.name == relative.parts[0]:
            # Preserve the broken symlinks; the exclusion below stops Cargo
            # from following them. Do not copy any other project's Git source.
            shutil.copytree(checkout, checkouts / checkout.name, symlinks=True)
        else:
            (checkouts / checkout.name).symlink_to(checkout, target_is_directory=True)

    copied_manifest = checkouts / relative
    text = copied_manifest.read_text()
    package = tomllib.loads(text)["package"]
    if (copied_manifest.parent / ".direnv").is_dir():
        if "exclude" not in package or "exclude = [" not in text:
            raise RuntimeError("tidlers manifest changed: review the .direnv vendoring workaround")
        text = text.replace('exclude = [', 'exclude = [\n    ".direnv/",', 1)
        copied_manifest.write_text(text)


if __name__ == "__main__":
    prepare_cache(Path(sys.argv[1]), Path(sys.argv[2]), json.loads(Path(sys.argv[3]).read_text()))
