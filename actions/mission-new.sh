#!/usr/bin/env bash
set -euo pipefail

# Interactive "新建 Team Mission" action: prompt for a title, then run the
# Rust runtime's two-phase create + launch.
ROOT="${HERDR_PLUGIN_ROOT:?}"
BIN="$ROOT/target/release/herdr-mission"

if [[ ! -x "$BIN" ]]; then
  echo "herdr-mission 未构建，请先安装/构建插件" >&2
  exit 1
fi

printf 'Mission 标题: '
read -r TITLE
if [[ -z "${TITLE:-}" ]]; then
  exit 0
fi

exec "$BIN" new --title="$TITLE"
