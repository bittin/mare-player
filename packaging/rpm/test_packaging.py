"""Fast, offline regression tests: python3 packaging/rpm/test_packaging.py."""

import copy
from pathlib import Path
import runpy
import shutil
import subprocess
import tempfile
import tomllib
import unittest
import xml.etree.ElementTree as ET


PACKAGING = Path(__file__).resolve().parent
prepare_cache = runpy.run_path(str(PACKAGING / "vendor-cache.py"))["prepare_cache"]


class VendorCacheTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.original = self.root / "cargo-home"
        self.destination = self.root / "private-cargo-home"
        (self.original / "registry").mkdir(parents=True)
        (self.original / "git/db").mkdir(parents=True)
        self.tidlers = self.original / "git/checkouts/tidlers-example/abcdef"
        self.tidlers.mkdir(parents=True)
        self.manifest = self.tidlers / "Cargo.toml"
        self.manifest.write_text(
            '[package]\nname = "tidlers"\nversion = "0.5.0"\n'
            'exclude = [\n    "downloads/",\n]\n'
        )
        (self.tidlers / "src").mkdir()
        (self.tidlers / "src/lib.rs").write_text("// unchanged dependency code\n")
        (self.original / "git/checkouts/another-crate/123456").mkdir(parents=True)
        self.metadata = {"packages": [{"name": "tidlers", "manifest_path": str(self.manifest)}]}

    def copied_manifest(self):
        return self.destination / "git/checkouts/tidlers-example/abcdef/Cargo.toml"

    def test_excludes_broken_nix_links_without_changing_original(self):
        nix = self.tidlers / ".direnv"
        nix.mkdir()
        (nix / "flake-profile").symlink_to("flake-profile-1-link")
        (nix / "flake-profile-1-link").symlink_to("/nonexistent/nix/store/example")
        original_text = self.manifest.read_text()
        metadata = copy.deepcopy(self.metadata)

        prepare_cache(self.original, self.destination, self.metadata)

        self.assertEqual(self.manifest.read_text(), original_text)
        self.assertEqual(self.metadata, metadata)
        package = tomllib.loads(self.copied_manifest().read_text())["package"]
        self.assertEqual(package["exclude"], [".direnv/", "downloads/"])
        self.assertTrue((nix / "flake-profile").is_symlink())
        self.assertEqual(
            (self.copied_manifest().parent / "src/lib.rs").read_bytes(),
            (self.tidlers / "src/lib.rs").read_bytes(),
        )
        # No complete duplicate of the developer's registry/other Git caches.
        self.assertEqual((self.destination / "registry").resolve(), self.original / "registry")
        self.assertTrue((self.destination / "git/checkouts/another-crate").is_symlink())
        self.assertFalse((self.destination / "git/checkouts/tidlers-example").is_symlink())

    def test_does_not_change_manifest_when_nix_directory_is_absent(self):
        prepare_cache(self.original, self.destination, self.metadata)
        self.assertEqual(self.copied_manifest().read_bytes(), self.manifest.read_bytes())

    def test_supports_a_symlinked_cargo_home(self):
        alias = self.root / "cargo-alias"
        alias.symlink_to(self.original, target_is_directory=True)
        relative = self.manifest.relative_to(self.original)
        self.metadata["packages"][0]["manifest_path"] = str(alias / relative)
        prepare_cache(alias, self.destination, self.metadata)
        self.assertEqual(self.copied_manifest().read_bytes(), self.manifest.read_bytes())

    def test_rejects_manifest_outside_the_cache(self):
        self.metadata["packages"][0]["manifest_path"] = str(self.root / "elsewhere/Cargo.toml")
        with self.assertRaises(ValueError):
            prepare_cache(self.original, self.destination, self.metadata)

    def test_fails_closed_if_upstream_manifest_needs_a_new_patch(self):
        (self.tidlers / ".direnv").mkdir()
        self.manifest.write_text('[package]\nname = "tidlers"\nversion = "0.5.0"\n')
        with self.assertRaisesRegex(RuntimeError, "manifest changed"):
            prepare_cache(self.original, self.destination, self.metadata)
        self.assertNotIn("exclude", self.manifest.read_text())


class CommandLineTests(unittest.TestCase):
    def run_helper(self, *args):
        return subprocess.run(
            ["bash", str(PACKAGING / "build-srpm.sh"), *args],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_help_does_not_build_or_upload(self):
        result = self.run_helper("--help")
        self.assertEqual(result.returncode, 0)
        self.assertIn("committed sources", result.stderr)

    def test_rejects_non_positive_or_rpm_macro_release_values(self):
        for release in ("0", "-1", "1.1", "%{lua:print(1)}", "1; echo bad"):
            with self.subTest(release=release):
                result = self.run_helper("HEAD", release)
                self.assertEqual(result.returncode, 2)
                self.assertIn("positive integer", result.stderr)

    def test_rejects_extra_arguments(self):
        self.assertEqual(self.run_helper("HEAD", "1", "unexpected").returncode, 2)


@unittest.skipUnless(shutil.which("rpmspec") and shutil.which("desktop-file-validate"),
                     "RPM and desktop-file-utils are needed for spec tests")
class SpecTests(unittest.TestCase):
    def test_both_variants_and_old_tag_desktop_metadata(self):
        template = (PACKAGING / "mare-player.spec.in").read_text()
        for applet, name, conflict in ((0, "mare-player", "cosmic-applet-mare"),
                                       (1, "cosmic-applet-mare", "mare-player")):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                spec = root / "package.spec"
                text = template
                for key, value in {"APPLET": str(applet), "VERSION": "0.3.4", "RELEASE": "1",
                                   "CHANGELOG_DATE": "Wed Sep 30 2026"}.items():
                    text = text.replace(f"@{key}@", value)
                spec.write_text(text)
                parsed = subprocess.check_output(["rpmspec", "--parse", str(spec)], text=True)
                self.assertIn(f"Name:           {name}", parsed)
                self.assertIn(f"Conflicts:      {conflict}", parsed)
                self.assertEqual("--no-default-features --features wgpu" in parsed, not applet)
                self.assertEqual("cargo build --release --frozen -p mare-video-window" in parsed, bool(applet))
                self.assertIn("Requires:       wl-clipboard", parsed)
                self.assertIn("%license LICENSE bundled-licenses", parsed)

                resources = root / "resources"
                resources.mkdir()
                for file in ("app.desktop", "app.metainfo.xml"):
                    shutil.copyfile(PACKAGING.parents[1] / "resources" / file, resources / file)
                desktop = resources / "app.desktop"
                # Exercise compatibility with release tags before the category fix.
                desktop.write_text(desktop.read_text().replace("AudioVideo;", ""))
                prep = parsed[parsed.index("# Older tags omitted"):parsed.index("%build")]
                subprocess.run(["bash", "-eu", "-c", prep], cwd=root, check=True)
                result = subprocess.run(["desktop-file-validate", str(desktop)],
                                        capture_output=True, text=True, check=True)
                self.assertNotIn("error:", result.stdout + result.stderr)
                self.assertIn(f"Exec={name} %u", desktop.read_text())
                self.assertIn(f"NoDisplay={str(bool(applet)).lower()}", desktop.read_text())
                self.assertEqual("X-CosmicApplet=true" in desktop.read_text(), bool(applet))
                self.assertIn("MimeType=x-scheme-handler/tidal;", desktop.read_text())
                self.assertEqual(ET.parse(resources / "app.metainfo.xml").find("provides/binary").text, name)


if __name__ == "__main__":
    unittest.main()
