#!/usr/bin/env bash
# fetch-or-build.sh — herdr [[build]] step for herdr-mission.
#
# Fast path: download the prebuilt binary matching this source's version + platform
# from the GitHub release, verify its SHA-256, and install it at target/release/herdr-mission.
# Fallback: on any miss (not the released commit, no asset, network/download error,
# checksum mismatch, unmapped platform, no curl/wget) build from source with cargo.
#
# Paths and the base URL are overridable via env (HM_REPO_ROOT / HM_CARGO_TOML / HM_OUT /
# HM_BASE_URL) so the logic is exercised by a hermetic test with stubbed tools.
set -u

repo="gxbsst/herdr-mission"

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root="${HM_REPO_ROOT:-$script_dir}"
cargo_toml="${HM_CARGO_TOML:-$repo_root/Cargo.toml}"
out="${HM_OUT:-$repo_root/target/release/herdr-mission}"
base_url="${HM_BASE_URL:-https://github.com/$repo/releases/download}"

have() { command -v "$1" >/dev/null 2>&1; }

build_from_source() {
  if [ -f "$HOME/.cargo/env" ]; then
    . "$HOME/.cargo/env"
  fi
  if ! have cargo; then
    echo "herdr-mission needs Rust 1.88+ to build, but cargo was not found." >&2
    echo "Install Rust from https://rustup.rs then re-run: herdr plugin install $repo" >&2
    exit 1
  fi
  exec cargo build --release --locked
}

fallback() {
  echo "herdr-mission: $1 — building from source instead." >&2
  [ -n "${tmpdir:-}" ] && rm -rf "$tmpdir"
  build_from_source
}

download() { # download <url> <dest>
  if have curl; then
    curl -fsSL -o "$2" "$1"
  elif have wget; then
    wget -q -O "$2" "$1"
  else
    return 127
  fi
}

# Download a release asset by name for the given tag. Prefers `gh release
# download` because it goes through the authenticated API (required for a
# private repo); falls back to the public direct URL via curl/wget.
release_asset() { # release_asset <tag> <asset-name> <dest-file>
  local tag="$1" asset="$2" dest="$3"
  if have gh && gh auth token >/dev/null 2>&1; then
    if gh release download "$tag" --repo "$repo" --pattern "$asset" --dir "$tmpdir" --clobber >/dev/null 2>&1; then
      if [ -f "$tmpdir/$asset" ]; then
        mv -f "$tmpdir/$asset" "$dest" && return 0
      fi
    fi
  fi
  download "$base_url/$tag/$asset" "$dest"
}

sha256_of() {
  if have sha256sum; then
    sha256sum "$1" | awk '{print $1}'
  elif have shasum; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    return 127
  fi
}

# Resolve the Rust target triple from the platform.
os=$(uname -s 2>/dev/null || echo unknown)
arch=$(uname -m 2>/dev/null || echo unknown)
triple=""
case "$os" in
  Darwin)
    case "$arch" in
      arm64 | aarch64) triple="aarch64-apple-darwin" ;;
      x86_64 | amd64) triple="x86_64-apple-darwin" ;;
    esac
    ;;
  Linux)
    case "$arch" in
      x86_64 | amd64) triple="x86_64-unknown-linux-musl" ;;
      aarch64 | arm64) triple="aarch64-unknown-linux-musl" ;;
    esac
    ;;
esac
[ -n "$triple" ] || fallback "no prebuilt binary for $os/$arch"

# Read the version this source declares.
version=$(grep -E '^version *= *"' "$cargo_toml" 2>/dev/null | head -n 1 | sed -E 's/^version *= *"([^"]+)".*/\1/')
[ -n "$version" ] || fallback "could not read version from $cargo_toml"

asset="herdr-mission-$triple"
tmpdir=$(mktemp -d 2>/dev/null) || fallback "could not create a temp dir"
trap 'rm -rf "$tmpdir"' EXIT

# Gate: this checkout must be exactly the commit the v$version release was built from.
# herdr clones a git work tree at the chosen commit but usually without local tags, so we
# compare HEAD to the COMMIT asset rather than resolving the tag locally. Between releases
# main sits at the last released version while HEAD has advanced; this makes the prebuilt
# fire only when the source IS the released commit.
if have git && git -C "$repo_root" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  head_rev=$(git -C "$repo_root" rev-parse HEAD 2>/dev/null || echo nohead)
  release_asset "v$version" "COMMIT" "$tmpdir/COMMIT" || fallback "release commit marker not available for v$version"
  release_commit=$(tr -d '[:space:]' < "$tmpdir/COMMIT" 2>/dev/null)
  if [ -z "$release_commit" ] || [ "$head_rev" != "$release_commit" ]; then
    fallback "checkout ($head_rev) is not the v$version release commit ($release_commit)"
  fi
fi

tmpbin="$tmpdir/$asset"
tmpsums="$tmpdir/SHA256SUMS"

release_asset "v$version" "$asset" "$tmpbin" || fallback "prebuilt binary not available for v$version ($asset)"
release_asset "v$version" "SHA256SUMS" "$tmpsums" || fallback "checksums not available for v$version"

expected=$(grep -E "^[0-9a-f]{64}  $asset\$" "$tmpsums" 2>/dev/null | awk '{print $1}' | head -n 1)
[ -n "$expected" ] || fallback "no checksum listed for $asset"

actual=$(sha256_of "$tmpbin") || fallback "no sha-256 tool (sha256sum/shasum) available"
if [ "$actual" != "$expected" ]; then
  fallback "checksum mismatch for $asset (expected $expected, got $actual)"
fi

chmod +x "$tmpbin"
mkdir -p "$(dirname "$out")"
mv -f "$tmpbin" "$out" || fallback "could not install the verified binary to $out"
echo "herdr-mission: installed prebuilt v$version ($triple), verified SHA-256."
exit 0
