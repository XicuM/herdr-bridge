#!/usr/bin/env bash
set -euo pipefail

# The bridge requires a bearer token (~/.config/herdr-bridge/token) on all requests except /health.
# Binding to 0.0.0.0 allows connections from localhost and Tailscale alike.
BIND_ADDR="${HERDR_BRIDGE_BIND:-0.0.0.0}"
PORT="${HERDR_BRIDGE_PORT:-7788}"

# Look for binary in standard local paths or repo build paths
BINARY=""
for candidate in \
  "$HOME/.local/bin/herdr-bridge" \
  "$(dirname "$0")/../bridge/target/release/herdr-bridge" \
  "$(dirname "$0")/../bridge/target/debug/herdr-bridge" \
  "$(which herdr-bridge 2>/dev/null || true)"
do
  if [[ -n "$candidate" && -x "$candidate" && ! -d "$candidate" ]]; then
    BINARY="$candidate"
    break
  fi
done

if [[ -z "$BINARY" ]]; then
  echo "Error: herdr-bridge binary not found." >&2
  exit 1
fi

exec "$BINARY" --bind "$BIND_ADDR" --port "$PORT" "$@"
