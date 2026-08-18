#!/usr/bin/env bash
set -euo pipefail

# Single delivery pass: wake any target role agent for queued outbox messages.
# Runs on herdr startup and on pane agent lifecycle events so assignment
# delivery is event-driven instead of relying on a long-running poller.
ROOT="${HERDR_PLUGIN_ROOT:?}"
BIN="$ROOT/target/release/herdr-mission"

if [[ ! -x "$BIN" ]]; then
  exit 0
fi

exec "$BIN" deliver --json
