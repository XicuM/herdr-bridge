#!/usr/bin/env bash
set -euo pipefail

# The bridge has no auth, so it listens on the Tailscale IP only: Tailscale is the security boundary.
# Until Tailscale is up this fails, and the service manager retries.
BIND_ADDR="${HERDR_BRIDGE_BIND:-$(tailscale ip -4 2>/dev/null | head -n1 || true)}"
if [[ -z "$BIND_ADDR" ]]; then
  echo "Error: no Tailscale IP found (is tailscale up?). Set HERDR_BRIDGE_BIND to choose an address." >&2
  exit 1
fi
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
