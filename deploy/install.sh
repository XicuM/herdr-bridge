#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
mkdir -p "$BIN_DIR"

# 1. Install binary
if [[ -f "$SCRIPT_DIR/../bridge/target/release/herdr-bridge" ]]; then
  install -m 755 "$SCRIPT_DIR/../bridge/target/release/herdr-bridge" "$BIN_DIR/herdr-bridge"
elif [[ -f "$SCRIPT_DIR/../bridge/target/debug/herdr-bridge" ]]; then
  install -m 755 "$SCRIPT_DIR/../bridge/target/debug/herdr-bridge" "$BIN_DIR/herdr-bridge"
elif which cargo >/dev/null 2>&1 && [[ -d "$SCRIPT_DIR/../bridge" ]]; then
  echo "Building herdr-bridge via cargo..."
  cargo build --release --manifest-path "$SCRIPT_DIR/../bridge/Cargo.toml"
  install -m 755 "$SCRIPT_DIR/../bridge/target/release/herdr-bridge" "$BIN_DIR/herdr-bridge"
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
  
  # Unload previous instance if running, then load
  launchctl bootout "gui/$(id -u)/dev.herdr.bridge" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$LAUNCH_AGENTS/dev.herdr.bridge.plist"
  echo "Started herdr-bridge background service via launchd (dev.herdr.bridge)"

elif [[ "$OS" == "Linux" ]]; then
  SYSTEMD_USER_DIR="$HOME/.config/systemd/user"
  mkdir -p "$SYSTEMD_USER_DIR"
  cp "$SCRIPT_DIR/herdr-bridge.service" "$SYSTEMD_USER_DIR/herdr-bridge.service"
  
  systemctl --user daemon-reload
  systemctl --user enable --now herdr-bridge.service
  systemctl --user restart herdr-bridge.service
  echo "Started herdr-bridge background service via systemd --user"
else
  echo "Warning: Unsupported OS for auto-service setup: $OS"
  echo "You can run the bridge manually using: $BIN_DIR/start-bridge.sh"
  exit 0
fi

# 3. Connection instructions for mobile
TAILSCALE_IP=$(tailscale ip -4 2>/dev/null || true)
echo ""
echo "=================================================="
echo " Herdr Bridge is now running in the background!"
if [[ -n "$TAILSCALE_IP" ]]; then
  echo " Tailscale IP : $TAILSCALE_IP"
  echo " Port         : 7788"
  echo " In the mobile app, connect to: $TAILSCALE_IP:7788"
else
  echo " Port         : 7788"
  echo " Tailscale was not detected. Enter your host IP:7788 in the mobile app."
fi
echo "=================================================="
