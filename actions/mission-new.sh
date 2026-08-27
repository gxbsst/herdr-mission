#!/usr/bin/env bash
set -euo pipefail

# Interactive "新建 Team Mission" action: prompt for a title and launch mode,
# then run the Rust runtime's two-phase create + launch.
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

printf '启动模式 [auto/manual，回车使用全局配置]: '
if ! read -r LAUNCH_MODE; then
  LAUNCH_MODE=""
fi

case "$LAUNCH_MODE" in
  auto|manual)
    ;;
  "")
    ;;
  *)
    echo "启动模式只接受 auto、manual 或回车使用全局配置" >&2
    exit 65
    ;;
esac

if [[ -n "${HERDR_PLUGIN_STATE_DIR:-}" ]]; then
  DB="$HERDR_PLUGIN_STATE_DIR/missions.sqlite3"
else
  DB="$HOME/.local/state/herdr/plugins/weston.herdr-mission/missions.sqlite3"
fi

if [[ -n "$LAUNCH_MODE" ]]; then
  exec "$BIN" new --title="$TITLE" --database="$DB" --launch-mode="$LAUNCH_MODE"
fi
exec "$BIN" new --title="$TITLE" --database="$DB"
