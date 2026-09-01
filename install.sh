#!/bin/sh

set -eu

repo="gxbsst/herdr-mission"
plugin_id="weston.herdr-mission"
skill_name="herdr-mission-team"
skill_asset="herdr-mission-team.skill.tar.gz"
embedded_release_tag="@HERDR_MISSION_RELEASE_TAG@"
release_tag="${HERDR_MISSION_RELEASE_TAG:-$embedded_release_tag}"
release_base_url="${HERDR_MISSION_RELEASE_BASE_URL:-https://github.com/$repo/releases/download}"
tty_path="${HERDR_MISSION_TTY_PATH:-/dev/tty}"
agents=""
assume_yes=false
tmpdir=""
cli_install_tmp=""

say() {
  printf '%s\n' "$*"
}

warn() {
  printf 'herdr-mission installer: warning: %s\n' "$*" >&2
}

die() {
  printf 'herdr-mission installer: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Install the Herdr Mission plugin, CLI, and Agent skill.

Usage: install.sh [--yes] [--agents codex|claude|codex,claude] [--version vX.Y.Z]

Options:
  --agents <selection>  Install the skill for Codex, Claude Code, or both.
  --version <tag>       Install one explicit release tag (for example v0.1.11).
  --yes                 Skip the final confirmation prompt.
  -h, --help            Show this help.
EOF
}

cleanup() {
  if [ -n "$cli_install_tmp" ]; then
    rm -f -- "$cli_install_tmp"
  fi
  if [ -n "$tmpdir" ]; then
    rm -rf -- "$tmpdir"
  fi
}

trap cleanup EXIT HUP INT TERM

while [ "$#" -gt 0 ]; do
  case "$1" in
    --agents)
      [ "$#" -ge 2 ] || die "--agents requires a value"
      agents=$2
      shift 2
      ;;
    --version | --release)
      [ "$#" -ge 2 ] || die "$1 requires a release tag"
      release_tag=$2
      shift 2
      ;;
    --yes | -y)
      assume_yes=true
      shift
      ;;
    --help | -h)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

case "$release_tag" in
  v[0-9]*.[0-9]*.[0-9]*) ;;
  *) die "release tag is missing or invalid; use a published install.sh or pass --version vX.Y.Z" ;;
esac
case "$release_tag" in
  *[!A-Za-z0-9._-]*) die "release tag contains unsupported characters: $release_tag" ;;
esac

if [ -z "$agents" ]; then
  [ -r "$tty_path" ] || die "cannot prompt for Agent selection; pass --agents codex,claude"
  printf '%s\n' "Install the $skill_name skill for:" >&2
  printf '%s\n' "  1) Codex" "  2) Claude Code" "  3) Both" >&2
  printf '%s' "Selection [3]: " >&2
  IFS= read -r selection < "$tty_path" || die "could not read Agent selection from $tty_path"
  case "$selection" in
    "" | 3 | both) agents="codex,claude" ;;
    1 | codex) agents="codex" ;;
    2 | claude) agents="claude" ;;
    *) die "invalid Agent selection: $selection" ;;
  esac
fi

case "$agents" in
  both | codex,claude | claude,codex) agents="codex,claude" ;;
  codex | claude) ;;
  *) die "--agents must be codex, claude, codex,claude, or both" ;;
esac

if [ "$assume_yes" != true ]; then
  [ -r "$tty_path" ] || die "cannot prompt for confirmation; pass --yes"
  printf 'Install Herdr Mission %s for %s? [y/N] ' "$release_tag" "$agents" >&2
  IFS= read -r confirmation < "$tty_path" || die "could not read confirmation from $tty_path"
  case "$confirmation" in
    y | Y | yes | YES) ;;
    *) die "installation cancelled" ;;
  esac
fi

command -v herdr >/dev/null 2>&1 || die "herdr is required; install Herdr before Herdr Mission"
command -v tar >/dev/null 2>&1 || die "tar is required"
command -v awk >/dev/null 2>&1 || die "awk is required"
command -v mktemp >/dev/null 2>&1 || die "mktemp is required"

if command -v curl >/dev/null 2>&1; then
  download_tool="curl"
elif command -v wget >/dev/null 2>&1; then
  download_tool="wget"
else
  die "curl or wget is required"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha_tool="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  sha_tool="shasum"
else
  die "sha256sum or shasum is required"
fi

os=$(uname -s 2>/dev/null || printf unknown)
arch=$(uname -m 2>/dev/null || printf unknown)
case "$os/$arch" in
  Darwin/arm64 | Darwin/aarch64) target="aarch64-apple-darwin" ;;
  Darwin/x86_64 | Darwin/amd64) target="x86_64-apple-darwin" ;;
  Linux/x86_64 | Linux/amd64) target="x86_64-unknown-linux-musl" ;;
  *) die "no prebuilt Herdr Mission CLI for $os/$arch" ;;
esac

cli_asset="herdr-mission-$target"
bin_dir="$HOME/.local/bin"
cli_path="$bin_dir/herdr-mission"
skill_parent="$HOME/.local/share/herdr-mission/skills"
canonical_skill="$skill_parent/$skill_name"
codex_skill="$HOME/.agents/skills/$skill_name"
claude_skill="$HOME/.claude/skills/$skill_name"
skill_owner="owner=herdr-mission-unified-installer-v1"

check_cli_target() {
  if [ -L "$cli_path" ] || { [ -e "$cli_path" ] && [ ! -f "$cli_path" ]; }; then
    die "CLI target is not a replaceable regular file: $cli_path"
  fi
}

check_cli_target

check_skill_target() {
  checked_target=$1
  checked_kind=$2
  if [ -L "$checked_target" ]; then
    die "skill path is an unexpected symlink: $checked_target"
  fi
  if [ ! -e "$checked_target" ]; then
    return 0
  fi
  [ -d "$checked_target" ] || die "skill path is not a directory: $checked_target"
  checked_marker="$checked_target/.installed-by-herdr-mission"
  checked_skill="$checked_target/SKILL.md"
  [ -f "$checked_marker" ] && [ ! -L "$checked_marker" ] || die "skill directory is not owned by this installer: $checked_target"
  if [ -e "$checked_skill" ] || [ -L "$checked_skill" ]; then
    [ -f "$checked_skill" ] && [ ! -L "$checked_skill" ] || die "skill directory has an invalid SKILL.md: $checked_target"
  fi
  checked_marker_owner=$(sed -n '1p' "$checked_marker")
  checked_marker_kind=$(sed -n '2p' "$checked_marker")
  checked_marker_lines=$(awk 'END { print NR + 0 }' "$checked_marker")
  if [ "$checked_marker_owner" != "$skill_owner" ] || [ "$checked_marker_kind" != "target=$checked_kind" ] || [ "$checked_marker_lines" -ne 2 ]; then
    die "skill directory is not owned by this installer: $checked_target"
  fi
}

check_selected_skill_targets() {
  check_skill_target "$canonical_skill" canonical
  case "$agents" in
    codex) check_skill_target "$codex_skill" codex ;;
    claude) check_skill_target "$claude_skill" claude ;;
    codex,claude)
      check_skill_target "$codex_skill" codex
      check_skill_target "$claude_skill" claude
      ;;
  esac
}

check_selected_skill_targets

tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/herdr-mission-install.XXXXXX") || die "could not create a temporary directory"
sums="$tmpdir/SHA256SUMS"
commit_file="$tmpdir/COMMIT"
cli_download="$tmpdir/$cli_asset"
skill_download="$tmpdir/$skill_asset"

download() {
  download_url=$1
  download_dest=$2
  if [ "$download_tool" = curl ]; then
    curl -fsSL -o "$download_dest" "$download_url"
  else
    wget -q -O "$download_dest" "$download_url"
  fi
}

asset_url="$release_base_url/$release_tag"
download "$asset_url/SHA256SUMS" "$sums" || die "could not download SHA256SUMS for $release_tag"
download "$asset_url/COMMIT" "$commit_file" || die "could not download COMMIT for $release_tag"
download "$asset_url/$cli_asset" "$cli_download" || die "could not download $cli_asset for $release_tag"
download "$asset_url/$skill_asset" "$skill_download" || die "could not download $skill_asset for $release_tag"

checksum_for() {
  checksum_name=$1
  checksum_count=$(awk -v name="$checksum_name" 'NF == 2 && $2 == name { count += 1 } END { print count + 0 }' "$sums")
  [ "$checksum_count" -eq 1 ] || die "expected exactly one checksum for $checksum_name"
  checksum_value=$(awk -v name="$checksum_name" 'NF == 2 && $2 == name { print $1 }' "$sums")
  [ "${#checksum_value}" -eq 64 ] || die "invalid checksum for $checksum_name"
  case "$checksum_value" in
    *[!0-9a-f]*) die "invalid checksum for $checksum_name" ;;
  esac
  printf '%s\n' "$checksum_value"
}

sha256_file() {
  if [ "$sha_tool" = sha256sum ]; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

verify_asset() {
  verify_name=$1
  verify_path=$2
  expected_checksum=$(checksum_for "$verify_name")
  actual_checksum=$(sha256_file "$verify_path") || die "could not hash $verify_name"
  [ "$actual_checksum" = "$expected_checksum" ] || die "checksum mismatch for $verify_name"
}

verify_asset "$cli_asset" "$cli_download"
verify_asset "$skill_asset" "$skill_download"

release_commit=$(tr -d '[:space:]' < "$commit_file")
[ "${#release_commit}" -eq 40 ] || die "invalid COMMIT for $release_tag"
case "$release_commit" in
  *[!0-9a-f]*) die "invalid COMMIT for $release_tag" ;;
esac

archive_entries=$(tar -tzf "$skill_download") || die "could not inspect $skill_asset"
found_skill=false
while IFS= read -r archive_entry; do
  case "$archive_entry" in
    "$skill_name/" | "$skill_name/SKILL.md") ;;
    "") ;;
    *) die "unexpected path in $skill_asset: $archive_entry" ;;
  esac
  if [ "$archive_entry" = "$skill_name/SKILL.md" ]; then
    found_skill=true
  fi
done <<EOF
$archive_entries
EOF
[ "$found_skill" = true ] || die "$skill_asset does not contain $skill_name/SKILL.md"

tar -xzf "$skill_download" -C "$tmpdir" || die "could not extract $skill_asset"
extracted_skill="$tmpdir/$skill_name/SKILL.md"
[ -f "$extracted_skill" ] && [ ! -L "$extracted_skill" ] || die "skill payload is not a regular SKILL.md"

verify_skill_frontmatter() {
  awk '
    NR == 1 {
      if ($0 != "---") exit 1
      in_frontmatter = 1
      next
    }
    in_frontmatter && $0 == "---" {
      in_frontmatter = 0
      closed = 1
      next
    }
    in_frontmatter && /^name:/ {
      if ($0 != "name: herdr-mission-team") exit 1
      names += 1
    }
    END {
      if (in_frontmatter || !closed || names != 1) exit 1
    }
  ' "$1"
}

verify_skill_frontmatter "$extracted_skill" || die "skill frontmatter has the wrong name"
skill_payload_checksum=$(sha256_file "$extracted_skill") || die "could not hash extracted skill"

chmod +x "$cli_download" || die "could not mark downloaded CLI executable"
cli_version_output=$("$cli_download" --version 2>/dev/null) || die "downloaded CLI failed its version check"
expected_version=${release_tag#v}
printf '%s\n' "$cli_version_output" | grep -Fq "\"binary_version\":\"$expected_version\"" || die "downloaded CLI version does not match $release_tag"

herdr plugin install "$repo" --ref "$release_tag" --yes || die "Herdr plugin installation failed"
plugin_state=$(herdr plugin list --plugin "$plugin_id" --json) || die "could not verify installed Herdr plugin"
plugin_matches=$(printf '%s\n' "$plugin_state" | awk \
  -v id_token="\"plugin_id\":\"$plugin_id\"" \
  -v type_token="\"type\":\"plugin_list\"" \
  -v kind_token="\"kind\":\"github\"" \
  -v owner_token="\"owner\":\"gxbsst\"" \
  -v repo_token="\"repo\":\"herdr-mission\"" \
  -v ref_token="\"requested_ref\":\"$release_tag\"" \
  -v commit_token="\"resolved_commit\":\"$release_commit\"" '
  function occurrences(value, token, count) {
    count = 0
    while ((offset = index(value, token)) != 0) {
      count += 1
      value = substr(value, offset + length(token))
    }
    return count
  }
  {
    lines += 1
    plugin_ids += occurrences($0, "\"plugin_id\":")
    if (occurrences($0, id_token) == 1 &&
        occurrences($0, type_token) == 1 &&
        occurrences($0, kind_token) == 1 &&
        occurrences($0, owner_token) == 1 &&
        occurrences($0, repo_token) == 1 &&
        occurrences($0, ref_token) == 1 &&
        occurrences($0, commit_token) == 1) {
      matches += 1
    }
  }
  END {
    if (lines == 1 && plugin_ids == 1 && matches == 1) print 1
    else print 0
  }
')
[ "$plugin_matches" -eq 1 ] || die "installed plugin does not resolve to $release_commit"

# The plugin command is an external hook boundary. Recheck every local target
# after it returns so a preflighted path replacement is never treated as owned.
check_cli_target
check_selected_skill_targets

mkdir -p "$bin_dir" || die "could not create $bin_dir"
cli_install_tmp=$(mktemp "$bin_dir/.herdr-mission.install.XXXXXX") || die "could not stage CLI in $bin_dir"
cp "$cli_download" "$cli_install_tmp" || die "could not stage the CLI"
chmod 0755 "$cli_install_tmp" || die "could not set CLI permissions"
mv -f "$cli_install_tmp" "$cli_path" || die "could not install CLI at $cli_path"
cli_install_tmp=""

install_skill_payload() (
  payload_target=$1
  payload_kind=$2
  payload_tmp=""
  cleanup_payload() {
    if [ -n "$payload_tmp" ]; then
      rm -f -- "$payload_tmp"
    fi
  }
  trap cleanup_payload EXIT HUP INT TERM

  cd -P "$payload_target" || die "could not anchor skill directory: $payload_target"
  check_skill_target . "$payload_kind"
  payload_tmp=$(mktemp "./.SKILL.md.install.XXXXXX") || die "could not stage skill copy"
  cp "$extracted_skill" "$payload_tmp" || die "could not stage skill copy"
  mv -f "$payload_tmp" ./SKILL.md || die "could not update skill copy: $payload_target"
  payload_tmp=""
)

install_new_skill_copy() {
  new_target=$1
  new_kind=$2
  "$cli_download" __install-skill-copy \
    --payload "$extracted_skill" \
    --target "$new_target" \
    --kind "$new_kind" || die "could not atomically publish fresh skill copy: $new_target"
  check_skill_target "$new_target" "$new_kind"
}

install_skill_copy() {
  skill_target=$1
  skill_kind=$2
  skill_target_parent=$(dirname "$skill_target")
  mkdir -p "$skill_target_parent" || die "could not create skill parent: $skill_target_parent"

  if [ ! -e "$skill_target" ] && [ ! -L "$skill_target" ]; then
    install_new_skill_copy "$skill_target" "$skill_kind"
    return 0
  fi

  check_skill_target "$skill_target" "$skill_kind"
  install_skill_payload "$skill_target" "$skill_kind"
}

install_skill_copy "$canonical_skill" canonical
case "$agents" in
  codex) install_skill_copy "$codex_skill" codex ;;
  claude) install_skill_copy "$claude_skill" claude ;;
  codex,claude)
    install_skill_copy "$codex_skill" codex
    install_skill_copy "$claude_skill" claude
    ;;
esac

installed_version_output=$("$cli_path" --version 2>/dev/null) || die "installed CLI failed its version check"
printf '%s\n' "$installed_version_output" | grep -Fq "\"binary_version\":\"$expected_version\"" || die "installed CLI version does not match $release_tag"
verify_skill_frontmatter "$canonical_skill/SKILL.md" || die "installed skill frontmatter verification failed"

verify_installed_skill_copy() {
  installed_skill_target=$1
  installed_skill_kind=$2
  check_skill_target "$installed_skill_target" "$installed_skill_kind"
  installed_skill_checksum=$(sha256_file "$installed_skill_target/SKILL.md") || die "could not hash installed skill copy"
  [ "$installed_skill_checksum" = "$skill_payload_checksum" ] || die "installed skill copy does not match $release_tag: $installed_skill_target"
}

verify_installed_skill_copy "$canonical_skill" canonical
case "$agents" in
  codex) verify_installed_skill_copy "$codex_skill" codex ;;
  claude) verify_installed_skill_copy "$claude_skill" claude ;;
  codex,claude)
    verify_installed_skill_copy "$codex_skill" codex
    verify_installed_skill_copy "$claude_skill" claude
    ;;
esac

case ":${PATH:-}:" in
  *":$bin_dir:"*) ;;
  *) warn "$bin_dir is not in PATH; add it before invoking herdr-mission directly" ;;
esac

say "Herdr Mission $release_tag installed."
say "CLI: $cli_path"
say "Agent skill copies: $agents (canonical: $canonical_skill)"
