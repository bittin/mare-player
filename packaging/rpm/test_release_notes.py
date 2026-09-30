"""Published-release baseline selection and git-cliff range regression tests."""

import json
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
HELPER = runpy.run_path(str(ROOT / ".github/scripts/release-notes.py"))
previous_release = HELPER["previous_release"]
generate_notes = HELPER["generate_notes"]
published_releases = HELPER["published_releases"]


def release(tag, **overrides):
    return {"tag_name": tag, "draft": False, "prerelease": False,
            "published_at": "2026-09-30T00:00:00Z", **overrides}


class PublishedReleaseTests(unittest.TestCase):
    def test_failed_tag_does_not_hide_changes_on_a_rerun(self):
        # v0.3.5 exists in Git, but was never published on GitHub.
        releases = [release("v0.3.6"), release("v0.3.4")]
        self.assertEqual(previous_release(releases, "v0.3.6", lambda tag: True), "v0.3.4")

    def test_ignores_current_future_draft_prerelease_and_nonstable_tags(self):
        releases = [release("v0.3.4"), release("v0.3.5", draft=True),
                    release("v0.3.5", prerelease=True), release("v0.3.5", published_at=None),
                    release("nightly"), release("v0.3.6"), release("v0.3.7")]
        self.assertEqual(previous_release(releases, "v0.3.6", lambda tag: True), "v0.3.4")

    def test_uses_version_order_and_only_ancestors(self):
        releases = [release("v0.3.9"), release("v0.3.4"), release("v0.3.10")]
        self.assertEqual(previous_release(releases, "v0.3.11", lambda tag: True), "v0.3.10")
        self.assertEqual(previous_release(releases, "v0.3.11", lambda tag: tag != "v0.3.10"), "v0.3.9")

    def test_first_release_has_no_baseline(self):
        self.assertIsNone(previous_release([], "v0.1.0", lambda tag: True))
        self.assertIsNone(previous_release([release("v0.1.1")], "v0.1.0", lambda tag: True))

    def test_rejects_nonstable_current_tag(self):
        with self.assertRaises(ValueError):
            previous_release([], "v0.3.6-rc.1", lambda tag: True)

    def test_collects_every_api_page(self):
        pages = [[release("v0.3.6")], [release("v0.3.4")]]
        with patch("subprocess.check_output", return_value=json.dumps(pages)) as command:
            self.assertEqual(published_releases("owner/repo"), pages[0] + pages[1])
            args = command.call_args.args[0]
            self.assertIn("--paginate", args)
            self.assertIn("--slurp", args)
            self.assertNotIn("--jq", args)

    def test_api_failure_does_not_fall_back_to_git_tags(self):
        with patch("subprocess.check_output", side_effect=subprocess.CalledProcessError(1, "gh")):
            with self.assertRaises(subprocess.CalledProcessError):
                published_releases("owner/repo")


@unittest.skipUnless(shutil.which("git-cliff"), "git-cliff is needed for renderer integration")
class ReleaseRenderingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = {**os.environ, "GIT_EDITOR": "true", "GIT_CONFIG_GLOBAL": os.devnull,
                    "GIT_CONFIG_SYSTEM": os.devnull, "GIT_CONFIG_COUNT": "0"}
        self.env.pop("GIT_CONFIG_PARAMETERS", None)
        self.git("init", "--quiet", "--initial-branch=main")
        shutil.copyfile(ROOT / "cliff.toml", self.root / "cliff.toml")
        self.git("add", "cliff.toml")
        for tag, message in (("v0.3.4", "fix: baseline fix"),
                             ("v0.3.5", "feat: included feature"),
                             ("v0.3.6", "fix: included fix"),
                             ("v0.3.7", "fix: future fix")):
            self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                     "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", message)
            self.git("-c", "tag.gpgsign=false", "tag", tag)

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, env=self.env, text=True)

    def test_renders_one_complete_range_without_future_commits(self):
        notes = generate_notes("owner/repo", "v0.3.6",
                               [release("v0.3.7"), release("v0.3.6"), release("v0.3.4")], self.root)
        self.assertIn("Included feature", notes)
        self.assertIn("Included fix", notes)
        self.assertNotIn("Baseline fix", notes)
        self.assertNotIn("Future fix", notes)
        self.assertEqual(notes.count("## Bug fixes"), 1)
        self.assertIn("https://github.com/owner/repo/compare/v0.3.4...v0.3.6", notes)

    def test_rejects_shallow_history_instead_of_silently_truncating_notes(self):
        with tempfile.TemporaryDirectory() as shallow:
            self.git("clone", "--quiet", "--depth=1", self.root.as_uri(), shallow)
            with self.assertRaisesRegex(ValueError, "full Git history"):
                generate_notes("owner/repo", "v0.3.7", [release("v0.3.4")], Path(shallow))

    def test_first_release_stops_at_its_tag(self):
        notes = generate_notes("owner/repo", "v0.3.4", [], self.root)
        self.assertIn("Baseline fix", notes)
        self.assertNotIn("Included feature", notes)
        self.assertNotIn("Future fix", notes)
        self.assertNotIn("Full Changelog", notes)


if __name__ == "__main__":
    unittest.main()
