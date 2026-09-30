"""Exercise the release workflow's real shell snippets without uploading anything.

Requires PyYAML and shasum, also installed by the source-RPM job.
"""

import base64
import fnmatch
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[2]
# BaseLoader preserves GitHub's `on` key and scalar expressions as strings.
WORKFLOW = yaml.load((ROOT / ".github/workflows/release.yml").read_text(), Loader=yaml.BaseLoader)
COPR = WORKFLOW["jobs"]["copr"]
SRPM = WORKFLOW["jobs"]["srpm"]
RELEASE = WORKFLOW["jobs"]["release"]


def snippet(name, job=COPR):
    return next(step["run"] for step in job["steps"] if step.get("name") == name)


def executable(path, text):
    path.write_text(text)
    path.chmod(0o755)


def run(script, root, **env):
    return subprocess.run(
        ["bash", "-e", "-u", "-o", "pipefail", "-c", script],
        cwd=root,
        env={**os.environ, **env},
        capture_output=True,
        text=True,
        check=False,
    )


class CoprWorkflowTests(unittest.TestCase):
    def test_publication_is_opt_in_and_waits_for_the_release(self):
        self.assertEqual(COPR["needs"], ["release", "provenance"])
        self.assertIn("github.repository == 'glima/mare-player'", COPR["if"])
        self.assertIn("startsWith(github.ref, 'refs/tags/v')", COPR["if"])
        self.assertIn("vars.COPR_ENABLED == 'true'", COPR["if"])
        self.assertEqual(COPR["permissions"], {"contents": "read"})
        self.assertEqual(COPR["concurrency"]["cancel-in-progress"], "false")
        checkout = next(step for step in SRPM["steps"] if step.get("uses", "").startswith("actions/checkout@"))
        self.assertEqual(checkout["with"]["ref"], "${{ github.sha }}")
        self.assertEqual(checkout["with"]["persist-credentials"], "false")

    def test_credential_is_only_exposed_to_the_steps_that_need_it(self):
        self.assertNotIn("COPR_CONFIG", WORKFLOW.get("env", {}))
        self.assertNotIn("COPR_CONFIG", COPR.get("env", {}))
        secret_steps = [step["name"] for step in COPR["steps"] if "COPR_CONFIG" in step.get("env", {})]
        self.assertEqual(secret_steps, ["Check publishing credential", "Submit both variants to COPR"])
        self.assertNotIn("COPR_CONFIG", SRPM.get("env", {}))
        for step in SRPM["steps"]:
            self.assertNotIn("COPR_CONFIG", step.get("env", {}))
        artifact = next(step for step in SRPM["steps"] if step.get("uses", "").startswith("actions/upload-artifact@"))
        self.assertEqual(artifact["with"]["path"], "source-rpms/*.src.rpm")

    def test_release_and_copr_share_one_source_artifact(self):
        # SRPM release assets must be built even when COPR publishing is disabled.
        self.assertNotIn("if", SRPM)
        self.assertEqual(RELEASE["needs"], ["build", "srpm"])
        source_upload = next(step for step in SRPM["steps"]
                             if step.get("uses", "").startswith("actions/upload-artifact@"))
        download = next(step for step in RELEASE["steps"]
                        if step.get("uses", "").startswith("actions/download-artifact@"))
        self.assertTrue(fnmatch.fnmatch(source_upload["with"]["name"], download["with"]["pattern"]))
        self.assertEqual(source_upload["with"]["overwrite"], "true")
        binary_upload = next(step for step in WORKFLOW["jobs"]["build"]["steps"]
                             if step.get("uses", "").startswith("actions/upload-artifact@"))
        self.assertTrue(binary_upload["with"]["name"].startswith("packages-"))
        self.assertEqual(binary_upload["with"]["overwrite"], "true")
        copr_download = next(step for step in COPR["steps"]
                             if step.get("uses", "").startswith("actions/download-artifact@"))
        self.assertEqual(copr_download["with"]["name"], source_upload["with"]["name"])
        self.assertEqual(copr_download["with"]["path"], "source-rpms")
        self.assertFalse(any("build-srpm.sh" in step.get("run", "") for step in COPR["steps"]))

    def test_source_rpms_are_flat_release_assets_with_checksums_and_provenance(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            originals = root / "target/srpm/commit-1"
            originals.mkdir(parents=True)
            for name in ("mare-player", "cosmic-applet-mare"):
                (originals / f"{name}-0.3.5-1.src.rpm").write_bytes(f"fixture for {name}".encode())
            result = run(snippet("Stage source RPMs", SRPM), root)
            self.assertEqual(result.returncode, 0, result.stderr)

            source_upload = next(step for step in SRPM["steps"]
                                 if step.get("uses", "").startswith("actions/upload-artifact@"))
            pattern = source_upload["with"]["path"]
            staged = list(root.glob(pattern))
            self.assertEqual(len(staged), 2)
            # upload-artifact preserves directories below the first wildcard.
            # A wildcard here would hide SRPMs from the release's top-level globs.
            upload_root = root / Path(pattern).parent
            self.assertNotIn("*", str(upload_root))
            release = root / "release"
            release.mkdir()
            for source in staged:
                self.assertEqual(source.read_bytes(), (originals / source.name).read_bytes())
                relative = source.relative_to(upload_root)
                self.assertEqual(relative.parts, (source.name,))
                shutil.copyfile(source, release / relative)
            for name in ("binary.rpm", "package.deb", "bundle.tar.gz"):
                (release / name).write_bytes(b"binary release fixture")

            result = run(snippet("Generate checksums", RELEASE), root,
                         GITHUB_OUTPUT=str(root / "outputs"))
            self.assertEqual(result.returncode, 0, result.stderr)
            sums = (release / "SHA256SUMS").read_bytes()
            checksums = {name: digest for digest, name in
                         (line.split() for line in sums.decode().splitlines())}
            for source in staged:
                self.assertEqual(checksums[source.name], hashlib.sha256(source.read_bytes()).hexdigest())
            # This output is passed to the SLSA generator, not just an unsigned file.
            encoded = (root / "outputs").read_text().removeprefix("hashes=").strip()
            self.assertEqual(base64.b64decode(encoded), sums)
            for step_name, field in (("Create GitHub release", "files"),
                                     ("Attest build provenance", "subject-path")):
                patterns = next(step["with"][field] for step in RELEASE["steps"]
                                if step.get("name") == step_name).splitlines()
                for source in staged:
                    self.assertTrue(any(fnmatch.fnmatch(f"release/{source.name}", glob) for glob in patterns))

    def test_missing_credential_fails_before_upload(self):
        result = run(snippet("Check publishing credential"), ROOT, COPR_CONFIG="")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Actions secret is missing", result.stdout)

    def test_tag_and_commit_checks_before_preparing_sources(self):
        for ref, tag_commit, succeeds in (
            ("refs/heads/main", "event-commit", False),
            ("refs/tags/v0.3.5-rc.1", "event-commit", False),
            ("refs/tags/v0.3.5", "moved-tag", False),
            ("refs/tags/v0.3.5", "event-commit", True),
        ):
            with self.subTest(ref=ref, tag_commit=tag_commit), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                (root / "bin").mkdir()
                executable(root / "bin/git", '#!/bin/sh\nif [ "$2" = HEAD ]; then echo event-commit; else echo "$TAG_COMMIT"; fi\n')
                (root / "packaging/rpm").mkdir(parents=True)
                (root / "packaging/rpm/build-srpm.sh").write_text('printf "%s\\n" "$@" > prepared-args\n')
                result = run(
                    snippet("Prepare source RPMs from the release tag", SRPM), root,
                    PATH=f"{root / 'bin'}:{os.environ['PATH']}",
                    GITHUB_REF=ref, TAG_COMMIT=tag_commit,
                )
                self.assertEqual(result.returncode == 0, succeeds, result.stderr)
                self.assertEqual((root / "prepared-args").exists(), succeeds)
                if succeeds:
                    self.assertEqual((root / "prepared-args").read_text().splitlines(), [ref, "1"])

    def test_upload_uses_private_temporary_config_and_cleans_it_on_success_or_failure(self):
        for exit_code in (0, 1):
            with self.subTest(exit_code=exit_code), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                (root / "bin").mkdir()
                (root / "runner-temp").mkdir()
                srpms = root / "source-rpms"
                srpms.mkdir(parents=True)
                for name in ("mare-player", "cosmic-applet-mare"):
                    (srpms / f"{name}-0.3.5-1.src.rpm").write_text("test fixture, not an RPM\n")
                # This replaces copr-cli on PATH. It records only non-secret
                # facts, and cannot contact COPR or read the real user config.
                executable(root / "bin/copr-cli", '''#!/usr/bin/env python3
import json, os, pathlib, stat, sys
config = pathlib.Path(sys.argv[2])
assert config.read_text() == "test-only-credential\\n"
assert "COPR_CONFIG" not in os.environ
pathlib.Path("invocation.json").write_text(json.dumps({
    "args": sys.argv[3:],
    "mode": stat.S_IMODE(config.stat().st_mode),
    "config": str(config),
}))
sys.exit(int(os.environ["MOCK_EXIT"]))
''')
                result = run(
                    snippet("Submit both variants to COPR"), root,
                    PATH=f"{root / 'bin'}:{os.environ['PATH']}",
                    RUNNER_TEMP=str(root / "runner-temp"),
                    COPR_CONFIG="test-only-credential",
                    COPR_PROJECT="limachaves/mare-player",
                    GITHUB_REF_NAME="v0.3.5",
                    GITHUB_STEP_SUMMARY=str(root / "summary.md"),
                    MOCK_EXIT=str(exit_code),
                )
                self.assertEqual(result.returncode, exit_code, result.stderr)
                call = json.loads((root / "invocation.json").read_text())
                self.assertEqual(call["mode"], 0o600)
                self.assertEqual(Path(call["config"]).parent, root / "runner-temp")
                self.assertFalse(Path(call["config"]).exists())
                self.assertEqual(call["args"][:6], ["build", "--nowait", "--enable-net", "off", "limachaves/mare-player",
                                                  "source-rpms/cosmic-applet-mare-0.3.5-1.src.rpm"])
                self.assertEqual(call["args"][6:], ["source-rpms/mare-player-0.3.5-1.src.rpm"])
                self.assertNotIn("test-only-credential", result.stdout + result.stderr)
                self.assertEqual((root / "summary.md").exists(), exit_code == 0)
                if exit_code == 0:
                    self.assertIn("not build success", (root / "summary.md").read_text())


if __name__ == "__main__":
    unittest.main()
