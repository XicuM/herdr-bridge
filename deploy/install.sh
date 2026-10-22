#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
PORT="${HERDR_BRIDGE_PORT:-7788}"
mkdir -p "$BIN_DIR"

# The bridge listens on this machine only, and `tailscale serve` brings the tailnet to it. --lan opens it to
# every network the machine is on instead, where the token crosses in clear text.
BIND=127.0.0.1
[[ "${1:-}" == "--lan" ]] && BIND=0.0.0.0

# 1. Install binary
if [[ -f "$SCRIPT_DIR/../target/release/herdr-bridge" ]]; then
  install -m 755 "$SCRIPT_DIR/../target/release/herdr-bridge" "$BIN_DIR/herdr-bridge"
elif [[ -f "$SCRIPT_DIR/../target/debug/herdr-bridge" ]]; then
  install -m 755 "$SCRIPT_DIR/../target/debug/herdr-bridge" "$BIN_DIR/herdr-bridge"
elif which cargo >/dev/null 2>&1 && [[ -d "$SCRIPT_DIR/.." ]]; then
  echo "Building herdr-bridge via cargo..."
  cargo build --release --manifest-path "$SCRIPT_DIR/../Cargo.toml"
  install -m 755 "$SCRIPT_DIR/../target/release/herdr-bridge" "$BIN_DIR/herdr-bridge"
else
  echo "Error: herdr-bridge binary not found. Please build it first with 'cargo build --release'." >&2
  exit 1
fi

# Also copy start wrapper
install -m 755 "$SCRIPT_DIR/start-bridge.sh" "$BIN_DIR/start-bridge.sh"

echo "Installed herdr-bridge to $BIN_DIR/herdr-bridge"

# 2. Configure background daemon based on OS
OS="$(uname -s)"
if [[ "$OS" == "Darwin" ]]; then
  LAUNCH_AGENTS="$HOME/Library/LaunchAgents"
  mkdir -p "$LAUNCH_AGENTS"
  cp "$SCRIPT_DIR/dev.herdr.bridge.plist" "$LAUNCH_AGENTS/dev.herdr.bridge.plist"
  plutil -replace EnvironmentVariables.HERDR_BRIDGE_BIND -string "$BIND" "$LAUNCH_AGENTS/dev.herdr.bridge.plist"

  # Unload previous instance if running, then load
  launchctl bootout "gui/$(id -u)/dev.herdr.bridge" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$LAUNCH_AGENTS/dev.herdr.bridge.plist"
  echo "Started herdr-bridge background service via launchd (dev.herdr.bridge)"

elif [[ "$OS" == "Linux" ]]; then
  SYSTEMD_USER_DIR="$HOME/.config/systemd/user"
  mkdir -p "$SYSTEMD_USER_DIR"
  cp "$SCRIPT_DIR/herdr-bridge.service" "$SYSTEMD_USER_DIR/herdr-bridge.service"
  if [[ "$BIND" == 0.0.0.0 ]]; then
    mkdir -p "$SYSTEMD_USER_DIR/herdr-bridge.service.d"
    printf '[Service]\nEnvironment=HERDR_BRIDGE_BIND=0.0.0.0\n' >"$SYSTEMD_USER_DIR/herdr-bridge.service.d/lan.conf"
  else
    rm -f "$SYSTEMD_USER_DIR/herdr-bridge.service.d/lan.conf"
  fi

  systemctl --user daemon-reload
  systemctl --user enable herdr-bridge.service
  systemctl --user restart herdr-bridge.service
  echo "Started herdr-bridge background service via systemd --user"
else
  echo "Warning: Unsupported OS for auto-service setup: $OS"
  echo "You can run the bridge manually using: $BIN_DIR/start-bridge.sh"
  exit 0
fi

# 3. Bring the tailnet to it. Serve keeps this across restarts; on Linux it needs root or `tailscale set --operator=$USER`.
TAILSCALE_IP=$(tailscale ip -4 2>/dev/null | head -n1 || true)
if [[ -n "$TAILSCALE_IP" && "$BIND" == 127.0.0.1 ]] && ! tailscale serve --bg --tcp "$PORT" "tcp://127.0.0.1:$PORT" >/dev/null; then
  echo "Warning: tailscale serve failed. Run: sudo tailscale serve --bg --tcp $PORT tcp://127.0.0.1:$PORT" >&2
elif [[ -n "$TAILSCALE_IP" && "$BIND" == 0.0.0.0 ]]; then
  # The bridge answers on the tailnet itself now.
  tailscale serve --tcp "$PORT" off >/dev/null 2>&1 || true
fi

# 4. Connection instructions for mobile
# Made on first start if missing; the app sends it with every request.
TOKEN=$("$BIN_DIR/herdr-bridge" --print-token)
echo ""
echo "=================================================="
echo " Herdr Bridge is now running in the background!"
if [[ -n "$TAILSCALE_IP" ]]; then
  echo " Tailscale IP : $TAILSCALE_IP"
  echo " Port         : $PORT"
  echo " In the mobile app, connect to: $TAILSCALE_IP:$PORT"
elif [[ "$BIND" == 0.0.0.0 ]]; then
  echo " Port         : $PORT"
  echo " Tailscale was not detected. Enter your host IP:$PORT in the mobile app."
  echo " Anyone on this network can read the token as it crosses: use it on trusted networks only."
else
  echo " Tailscale was not detected, so only this machine can reach the bridge."
  echo " Install Tailscale and run this again, or run it with --lan to open it to this network."
fi
echo " Token        : $TOKEN"
echo "   (herdr-bridge --print-token shows it again)"
echo "=================================================="
