#!/usr/bin/env bash
set -u

# Interactive mission control center powered by the ratatui TUI. The full
# mission lifecycle (list / search / new / resume / send / deliver / doctor)
# is driven in-process by `herdr-mission tui`; q quits.
ROOT="${HERDR_PLUGIN_ROOT:?}"
BIN="$ROOT/target/release/herdr-mission"

if [[ ! -x "$BIN" ]]; then
  echo "herdr-mission 未构建，请先构建插件" >&2
  exit 1
fi

if [[ -n "${HERDR_PLUGIN_STATE_DIR:-}" ]]; then
  DB="$HERDR_PLUGIN_STATE_DIR/missions.sqlite3"
else
  DB="$HOME/.local/share/herdr-mission/missions.sqlite3"
fi

exec "$BIN" tui --database="$DB"
