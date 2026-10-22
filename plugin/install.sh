#!/usr/bin/env bash
# The herdr plugin's commands (herdr-plugin.toml): installing or updating the plugin runs --bridge, its panes
# --app, --phone and --status, and the bridge runs --uninstall itself once the plugin is uninstalled.
#   install.sh --bridge | --app | --phone | --status | --uninstall
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
LIB_DIR="$HOME/.local/lib/herdr-bridge"
PORT=7788
# The bridge is released here, the app and the APK by herdr-mobile.
BRIDGE_RELEASES="https://github.com/XicuM/herdr-bridge/releases/latest/download"
RELEASES="https://github.com/XicuM/herdr-mobile/releases/latest/download"
OS="$(uname -s)"
ARCH="$(uname -m)"
[[ "$ARCH" == arm64 ]] && ARCH=aarch64

# A herdr popup closes with its command, so leave what it printed up until it's read.
[[ -n "${HERDR_PLUGIN_ID:-}" ]] && trap 'read -rp "Press Enter to close" _' EXIT

# The bridge listens on this machine only, and `tailscale serve` brings the tailnet to it, unless the service
# was told otherwise (`systemctl --user edit herdr-bridge`: Environment=HERDR_BRIDGE_BIND=0.0.0.0).
BIND=127.0.0.1
[[ "$OS" == Linux ]] && systemctl --user show -p Environment herdr-bridge 2>/dev/null | grep -q 'HERDR_BRIDGE_BIND=0.0.0.0' && BIND=0.0.0.0

case "${1:-}" in
--bridge)
  mkdir -p "$BIN_DIR" "$LIB_DIR"
  # The release's binary, else a build from source (macOS has no release binary, and a download can fail).
  if [[ "$OS" == Linux ]] && echo "Downloading herdr-bridge for $ARCH..." &&
    curl -fL --progress-bar -o "$BIN_DIR/herdr-bridge.new" "$BRIDGE_RELEASES/herdr-bridge-linux-$ARCH"; then
    chmod 755 "$BIN_DIR/herdr-bridge.new"
    mv "$BIN_DIR/herdr-bridge.new" "$BIN_DIR/herdr-bridge"
  elif command -v cargo >/dev/null; then
    rm -f "$BIN_DIR/herdr-bridge.new"
    echo "Building herdr-bridge via cargo..."
    cargo build --release --manifest-path "$SCRIPT_DIR/../Cargo.toml"
    install -m 755 "$SCRIPT_DIR/../target/release/herdr-bridge" "$BIN_DIR/herdr-bridge"
  else
    echo "Error: no herdr-bridge binary for $OS; install Rust (cargo) to build it." >&2
    exit 1
  fi
  # herdr may prune the plugin's files once it is uninstalled, and the bridge runs this then.
  install -m 755 "$SCRIPT_DIR/install.sh" "$LIB_DIR/install.sh"
  rm -f "$BIN_DIR/start-bridge.sh" # The wrapper earlier versions started the bridge through.
  echo "Installed herdr-bridge to $BIN_DIR/herdr-bridge"

  if [[ "$OS" == Darwin ]]; then
    mkdir -p "$HOME/Library/LaunchAgents"
    cp "$SCRIPT_DIR/dev.herdr.bridge.plist" "$HOME/Library/LaunchAgents/dev.herdr.bridge.plist"
    launchctl bootout "gui/$(id -u)/dev.herdr.bridge" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/dev.herdr.bridge.plist"
    echo "Started herdr-bridge via launchd (dev.herdr.bridge)"
  else
    mkdir -p "$HOME/.config/systemd/user"
    cp "$SCRIPT_DIR/herdr-bridge.service" "$HOME/.config/systemd/user/herdr-bridge.service"
    systemctl --user daemon-reload
    systemctl --user enable herdr-bridge.service
    systemctl --user restart herdr-bridge.service
    echo "Started herdr-bridge via systemd --user"
  fi

  # Serve keeps this across restarts; on Linux it needs root or `tailscale set --operator=$USER`.
  if tailscale status >/dev/null 2>&1; then
    if [[ "$BIND" == 0.0.0.0 ]]; then
      # The bridge answers on the tailnet itself.
      tailscale serve --tcp "$PORT" off >/dev/null 2>&1 || true
    elif ! tailscale serve --bg --tcp "$PORT" "tcp://127.0.0.1:$PORT" >/dev/null; then
      echo "Warning: tailscale serve failed. Run: sudo tailscale serve --bg --tcp $PORT tcp://127.0.0.1:$PORT" >&2
    fi
  elif [[ "$BIND" == 127.0.0.1 ]]; then
    echo "Tailscale isn't running, so only this machine can reach the bridge. Start Tailscale and update the plugin."
  fi
  ;;

--app)
  if [[ "$OS" != Linux ]]; then
    echo "The desktop app is built for Linux only." >&2
    exit 1
  fi
  echo "Downloading the herdr-mobile app for $ARCH..."
  tmp="$(mktemp -d)"
  curl -fL --progress-bar "$RELEASES/herdr-mobile-linux-$ARCH.tar.gz" | tar -xz -C "$tmp"
  rm -rf "$HOME/.local/lib/herdr-mobile"
  mkdir -p "$BIN_DIR" "$HOME/.local/lib" "$HOME/.local/share/applications"
  mv "$tmp/herdr-mobile" "$HOME/.local/lib/herdr-mobile"
  rmdir "$tmp"
  ln -sf "$HOME/.local/lib/herdr-mobile/herdr_mobile" "$BIN_DIR/herdr-mobile"
  cat >"$HOME/.local/share/applications/herdr-mobile.desktop" <<EOF
[Desktop Entry]
Name=Herdr Mobile
Comment=Companion app for Herdr terminal multiplexer
Exec=$BIN_DIR/herdr-mobile
Terminal=false
Type=Application
Categories=Development;Utility;
Keywords=herdr;terminal;ai;
EOF
  echo "Installed the app to ~/.local/lib/herdr-mobile; run herdr-mobile or open it from your launcher."
  ;;

--phone)
  # The address the phone reaches: the tailnet's, else this network's when the bridge listens on it.
  HOST=
  tailscale status >/dev/null 2>&1 && HOST="$(tailscale ip -4 2>/dev/null | head -n1)"
  if [[ -z "$HOST" && "$BIND" == 0.0.0.0 ]]; then
    HOST="$(ip -4 route get 1.1.1.1 | awk '{for (i = 1; i < NF; i++) if ($i == "src") print $(i + 1)}')"
  fi
  # Made on first start; the app sends it with every request.
  TOKEN="$("$BIN_DIR/herdr-bridge" --print-token 2>/dev/null || true)"
  qr() { if command -v qrencode >/dev/null; then qrencode -t ansiutf8 -m 2 "$1"; fi; echo "  $1"; }

  echo ""
  echo "1. Install the app: scan this with the phone's camera, or open the link on the phone."
  qr "$RELEASES/app-release.apk"
  if [[ -z "$TOKEN" ]]; then
    echo "2. The bridge isn't installed here: update the plugin (herdr plugin update xicum.herdr-bridge)." >&2
  elif [[ -z "$HOST" ]]; then
    echo "2. The bridge isn't reachable from the phone: start Tailscale, update the plugin and run this again." >&2
  else
    echo ""
    echo "2. Pair: in the app, Machines (top right) ➔ + ➔ Scan QR, or paste the link into Address."
    echo "   It holds this machine's token: don't share it or a screenshot of it."
    qr "herdrmobile://pair?to=$HOST:$PORT&token=${TOKEN//-/}&name=$(hostname -s)"
    echo "   Or type it in: address $HOST:$PORT, token $TOKEN"
    command -v qrencode >/dev/null || echo "   (Install qrencode to show these as QR codes.)"
  fi
  ;;

--status)
  if [[ "$OS" == Darwin ]]; then
    launchctl print "gui/$(id -u)/dev.herdr.bridge" | head -20 || true
    tail -n 30 "$HOME/Library/Logs/herdr-bridge.log" || true
  else
    systemctl --user status herdr-bridge --no-pager || true
    journalctl --user -u herdr-bridge -n 30 --no-pager || true
  fi
  tailscale serve status 2>/dev/null || true
  ;;

--uninstall)
  # The bridge runs this from its own service, so the service is stopped last: that ends this script too.
  # The token stays, so a phone paired with this machine pairs again after a reinstall.
  tailscale serve --tcp "$PORT" off >/dev/null 2>&1 || true
  rm -rf "$BIN_DIR/herdr-bridge" "$BIN_DIR/start-bridge.sh" "$LIB_DIR" \
    "$HOME/.local/lib/herdr-mobile" "$BIN_DIR/herdr-mobile" "$HOME/.local/share/applications/herdr-mobile.desktop"
  echo "Removed herdr-bridge, and herdr-mobile's Linux app if it was installed; the token stays in ~/.config/herdr-bridge."
  if [[ "$OS" == Darwin ]]; then
    rm -f "$HOME/Library/LaunchAgents/dev.herdr.bridge.plist"
    launchctl bootout "gui/$(id -u)/dev.herdr.bridge" 2>/dev/null || true
  else
    systemctl --user disable herdr-bridge.service 2>/dev/null || true
    rm -rf "$HOME/.config/systemd/user/herdr-bridge.service" "$HOME/.config/systemd/user/herdr-bridge.service.d"
    systemctl --user daemon-reload
    systemctl --user stop herdr-bridge.service 2>/dev/null || true
  fi
  ;;

*)
  echo "Usage: $0 --bridge | --app | --phone | --status | --uninstall" >&2
  exit 2
  ;;
esac
