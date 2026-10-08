#!/usr/bin/env bash
# Build COPR-ready SRPMs without compiling or publishing anything.
set -euo pipefail

usage() {
    echo "Usage: bash packaging/rpm/build-srpm.sh [GIT_REF=HEAD] [RPM_RELEASE=1]" >&2
    echo "Packages committed sources at GIT_REF using the current RPM template." >&2
    echo "Writes both variants to target/srpm/<commit>-<release>/." >&2
}

if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
fi
if (( $# > 2 )); then
    usage
    exit 2
fi
ref=${1:-HEAD}
release=${2:-1}
if [[ ! "$release" =~ ^[1-9][0-9]*$ ]]; then
    echo "RPM_RELEASE must be a positive integer (increment it for updates at the same Cargo version)." >&2
    exit 2
fi

for tool in git cargo python3 tar xz rpmbuild; do
    command -v "$tool" >/dev/null || { echo "Missing prerequisite: $tool" >&2; exit 1; }
done
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
commit=$(git -C "$repo" rev-parse --verify --end-of-options "${ref}^{commit}")
version=$(git -C "$repo" show "$commit:Cargo.toml" | python3 -c '
import sys, tomllib
print(tomllib.loads(sys.stdin.read())["package"]["version"])
')
# Keep RPM ordering unambiguous. Prereleases need an explicit SemVer-to-RPM
# policy; do not accidentally publish one as a stable update.
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Only stable x.y.z Cargo versions are supported; got: $version" >&2
    exit 1
fi
tag=${ref#refs/tags/}
if [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ && "$tag" != "v$version" ]]; then
    echo "Release tag $ref does not match Cargo.toml version $version." >&2
    exit 1
fi

output="$repo/target/srpm/$commit-$release"
if [[ -e "$output" ]]; then
    echo "Output already exists: $output (use a higher RPM_RELEASE for a rebuild)." >&2
    exit 1
fi
mkdir -p "$repo/target/srpm"
build=$(mktemp -d "$repo/target/srpm/.build.XXXXXXXX")
trap 'rm -rf -- "$build"' EXIT
mkdir -p "$build/SOURCES" "$build/SPECS" "$build/SRPMS"

# git archive excludes uncommitted changes, build products and user secrets.
# The selected application's source is untouched; packaging uses this
# checkout's template so packaging fixes can be applied to an existing tag.
git -C "$repo" archive --format=tar --prefix="mare-player-$version/" "$commit" > "$build/source.tar"
tar -xf "$build/source.tar" -C "$build"
source_dir="$build/mare-player-$version"
printf '%s\n' "$commit" > "$source_dir/RPM-SOURCE-REVISION"

(
    cd "$source_dir"
    if [[ -e .cargo/config || -e .cargo/config.toml ]]; then
        echo "Source already has a Cargo config; merge it with vendor configuration before packaging." >&2
        exit 1
    fi
    # Resolve/fetch first, then vendor offline from a disposable cache view.
    # tidlers currently tracks broken Nix .direnv symlinks; its packaging-only
    # exclusion is applied to a COPY, never to the user's Cargo cache.
    # metadata alone omits inactive optional dependencies (notably wgpu).
    # fetch covers the entire lockfile so a cold cache works for BOTH variants.
    cargo fetch --locked
    cargo metadata --frozen --format-version 1 > "$build/metadata.json"
    python3 "$repo/packaging/rpm/vendor-cache.py" \
        "${CARGO_HOME:-$HOME/.cargo}" "$build/vendor-cargo-home" "$build/metadata.json"
    # Save Cargo's complete source replacement map (including Git sources).
    CARGO_HOME="$build/vendor-cargo-home" cargo vendor --frozen --versioned-dirs vendor > "$build/vendor-config.toml"
    mkdir -p .cargo
    mv "$build/vendor-config.toml" .cargo/config.toml
    # Validate with an empty cache: the eventual COPR build must be offline.
    CARGO_HOME="$build/empty-cargo-home" cargo metadata --frozen --format-version 1 > /dev/null
)

# Normalize archive metadata. Each SRPM includes the complete corresponding
# source and dependency licenses, not just a prebuilt application binary.
epoch=$(git -C "$repo" show -s --format=%ct "$commit")
changelog_date=$(LC_ALL=C date -u -d "@$epoch" '+%a %b %d %Y')
tar --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
    -cJf "$build/SOURCES/mare-player-$version-vendored.tar.xz" \
    -C "$build" "mare-player-$version"

for variant in standalone applet; do
    applet=0
    name=mare-player
    if [[ "$variant" == applet ]]; then
        applet=1
        name=cosmic-applet-mare
    fi
    spec="$build/SPECS/$name.spec"
    sed -e "s/@VERSION@/$version/g" -e "s/@RELEASE@/$release/g" \
        -e "s/@APPLET@/$applet/g" -e "s/@CHANGELOG_DATE@/$changelog_date/g" \
        "$repo/packaging/rpm/mare-player.spec.in" > "$spec"
    rpmbuild -bs --define "_topdir $build" "$spec"
done

# Publish the local output only once both variants have succeeded.
mv "$build/SRPMS" "$output"
printf '\nSource RPMs ready (nothing uploaded):\n'
find "$output" -maxdepth 1 -name '*.src.rpm' -print
