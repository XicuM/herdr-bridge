#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Auto-detect Tailscale IP if available, otherwise bind to 0.0.0.0
TAILSCALE_IP=$(tailscale ip -4 2>/dev/null || true)
BIND_ADDR="${TAILSCALE_IP:-0.0.0.0}"
PORT="${HERDR_BRIDGE_PORT:-7788}"

BINARY="$ROOT_DIR/bridge/target/release/herdr-bridge"
if [[ ! -f "$BINARY" ]]; then
  BINARY="$ROOT_DIR/bridge/target/debug/herdr-bridge"
fi

if [[ ! -f "$BINARY" ]]; then
  echo "Error: Binary not found. Run 'cargo build --release' inside $ROOT_DIR/bridge first."
  exit 1
fi

echo "=================================================="
echo " Starting Herdr Mobile Bridge Daemon"
echo " Bind Address: http://${BIND_ADDR}:${PORT}"
echo " Herdr Socket: ${HERDR_SOCKET:-$HOME/.config/herdr/herdr.sock}"
echo "=================================================="

exec "$BINARY" --bind "$BIND_ADDR" --port "$PORT" "$@"
