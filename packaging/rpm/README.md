# Fedora COPR channel

This is the maintainer guide for
[limachaves/mare-player](https://copr.fedorainfracloud.org/coprs/limachaves/mare-player/).
Local source-RPM preparation never uploads anything. GitHub release binaries
remain unchanged; an optional release job submits source RPMs to COPR once
[automatic publishing is explicitly enabled](#automatic-github-release-publishing).

The manual path is:

1. Prepare source RPMs (SRPMs) locally from a chosen Git commit or release tag.
2. Upload them to COPR.
3. COPR compiles them with Fedora's toolchain, signs the binary RPMs, and hosts
   the repository and signing key. Users enable it once and get DNF updates.

No domain, server, or maintainer-managed RPM signing key is needed. COPR builds
are public: submit only source you intend to publish. COPR's legal/content
requirements still apply; do not bundle proprietary codecs or TIDAL content.

## 1. Prepare the source RPMs

From this repository:

```sh
# Use a reviewed release tag whose Cargo version matches the tag.
just build-srpm v0.3.4
# Equivalent without just:
# bash packaging/rpm/build-srpm.sh v0.3.4 1
```

`HEAD` is the default for local testing if no ref is supplied; prefer a reviewed
release tag for publishing. Only **committed sources at the selected ref** are
included, not working-tree changes. The RPM template and
vendoring helper come from your current checkout, allowing packaging fixes for
an existing application release. The source archive records the selected
commit in `RPM-SOURCE-REVISION`.

The helper fetches dependencies pinned by `Cargo.lock`, vendors registry and
Git sources, and verifies resolution offline with an empty Cargo cache. The
pinned `tidlers` currently contains broken `.direnv` symlinks into its author's
Nix store. `vendor-cache.py` adds a packaging-only exclusion in a **private copy**
of that checkout before vendoring. It does not change dependency code, the
lockfile, or your Cargo cache. Remove that workaround when upstream fixes it.

Output goes into `target/srpm/<full-commit>-<rpm-release>/`:

- `mare-player-<version>-<release>.<dist>.src.rpm`
- `cosmic-applet-mare-<version>-<release>.<dist>.src.rpm`

Both contain the complete vendored source; neither contains a prebuilt binary.
COPR reevaluates `%{?dist}` for each target Fedora release. The applet build
includes `mare-video-window`; the standalone build enables `wgpu` and installs
a visible launcher pointing to `mare-player`.

The helper refuses to overwrite an existing output directory. For a
packaging-only update at the same Cargo version, increment the RPM release:

```sh
# 0.3.4-3 was the first COPR upload; choose a release newer than the latest one.
just build-srpm v0.3.4 4
```

For the next application version, use its tag and start at release `1` again.
Always increase version/release relative to what you published—DNF will not
upgrade users to a rebuild with the same version/release. This includes **source
snapshots at different commits with the same Cargo version**, not just packaging
fixes: the output-directory guard cannot know what is already on COPR. Only
stable `x.y.z` versions are supported; prerelease ordering needs a separate policy.

## 2. Validate, then submit

For a clean local Fedora build, use Mock (installation/group setup is described
in [Mock's documentation](https://rpm-software-management.github.io/mock/)):

```sh
# Replace this with the exact output directory printed by build-srpm.
SRPM_DIR=target/srpm/REPLACE_WITH_COMMIT-1
mock -r fedora-44-x86_64 --rebuild "$SRPM_DIR"/mare-player-*.src.rpm
mock -r fedora-44-x86_64 --rebuild "$SRPM_DIR"/cosmic-applet-mare-*.src.rpm
```

A successful SRPM creation alone does not establish binary-build or runtime
compatibility. The binary RPM carries the license/NOTICE files and crate
manifests supplied in the vendored sources, but collecting files is not a
license audit. Follow the project's dependency-license checks and review any
upstream notice omissions before public distribution.

After a clean build and that review, explicitly submit both source packages:

```sh
COPR_OWNER=limachaves
for srpm in "$SRPM_DIR"/*.src.rpm; do
    copr-cli build --nowait --enable-net off "$COPR_OWNER/mare-player" "$srpm"
done
```

Alternatively, use COPR's web UI to upload each SRPM. Check both builds and all
selected chroots before advertising the repository. Compilation needs a recent
Fedora Rust toolchain; Rust 2024's edition minimum alone is not a guarantee that
all pinned dependencies support an older compiler.

## 3. Test the user experience

Once COPR reports successful builds, use a clean Fedora machine/VM:

```sh
COPR_OWNER=limachaves
sudo dnf copr enable "$COPR_OWNER/mare-player"
sudo dnf install mare-player
```

For the applet, install `cosmic-applet-mare` **instead** (it requires COSMIC's
panel). To switch an existing installation:

```sh
sudo dnf swap mare-player cosmic-applet-mare
# Reverse the names to switch back to the standalone app.
```

Validate before announcing the channel:

- Install each alternative; verify the standalone launcher is visible and the
  applet can be added through COSMIC panel settings.
- Sign in, check the browser's `tidal://` callback, play audio, and open a video.
  The RPM registers a URI handler but never rewrites a user's preferred handler.
  If another app owns it, use the `xdg-mime` command in the main README **as the
  user, not root**.
- Test playback with the runtime GStreamer plugins. Full AAC/H.264 support may
  require [RPM Fusion](https://rpmfusion.org/Howto/Multimedia); COPR does not
  enable RPM Fusion, redistribute its codecs, or bypass codec licensing rules.
- Publish a higher version/release, then check `sudo dnf upgrade --refresh`.
- Test removal and switching variants, including the applet video companion.

Only after that should the main installation documentation advertise an actual
`OWNER/mare-player` COPR repository.

## Packaging regression checks

```sh
# Workflow tests also need PyYAML: sudo dnf install python3-pyyaml
python3 -B -m unittest discover -s packaging/rpm -p 'test_*.py'
shellcheck packaging/rpm/build-srpm.sh
# If installed:
actionlint .github/workflows/release.yml
```

These fast offline checks cover the private-cache workaround, argument
validation, release/tag gates, and upload credential handling. The upload tests
use a fake client and never contact COPR. When `rpmspec` and
`desktop-file-utils` are installed, they also check both rendered specs, feature
flags, conflicts, and desktop/URI metadata (including older tags). They
complement, not replace, the real SRPM/Mock/install tests above.

## Automatic GitHub release publishing

The `copr` job in `.github/workflows/release.yml` runs **after the GitHub release
and its SLSA provenance job succeed**. It uses the same source-RPM helper as the
manual route, at the exact release commit, and submits both variants to
`limachaves/mare-player` with networking disabled. COPR chooses the build targets
from its project configuration; there is no second Fedora/architecture matrix
to maintain in GitHub.

It is part of the existing workflow, not a separate `release: published`
workflow: GitHub does not trigger another workflow for a release created by
`GITHUB_TOKEN`.
