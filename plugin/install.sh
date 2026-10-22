#!/usr/bin/env bash
# Sets up herdr-bridge on this machine (where herdr runs), herdr-mobile's Linux app, and pairing a phone.
#   install.sh [--bridge] [--app] [--phone] [--lan] [--status] [--uninstall]
# With none of --bridge, --app, --phone, --status or --uninstall it asks which, or in a script sets up the bridge.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
PORT="${HERDR_BRIDGE_PORT:-7788}"
# The bridge is released here, the app and the APK by herdr-mobile.
BRIDGE_RELEASES="https://github.com/XicuM/herdr-bridge/releases/latest/download"
RELEASES="https://github.com/XicuM/herdr-mobile/releases/latest/download"
OS="$(uname -s)"
ARCH="$(uname -m)"
[[ "$ARCH" == arm64 ]] && ARCH=aarch64
mkdir -p "$BIN_DIR"

# A herdr popup closes with its command, so leave what it printed up until it's read.
[[ -n "${HERDR_PLUGIN_ID:-}" ]] && trap 'read -rp "Press Enter to close" _' EXIT

BRIDGE= APP= PHONE= STATUS= UNINSTALL=
# The bridge listens on this machine only, and `tailscale serve` brings the tailnet to it. --lan opens it to
# every network the machine is on instead, where the token crosses in clear text.
BIND=127.0.0.1
[[ -f "$HOME/.config/systemd/user/herdr-bridge.service.d/lan.conf" ]] && BIND=0.0.0.0
for arg in "$@"; do
  case "$arg" in
    --bridge) BRIDGE=1 ;;
    --app) APP=1 ;;
    --phone) PHONE=1 ;;
    --lan) BIND=0.0.0.0 ;;
    --status) STATUS=1 ;;
    --uninstall) UNINSTALL=1 ;;
    *) echo "Usage: $0 [--bridge] [--app] [--phone] [--lan] [--status] [--uninstall]" >&2; exit 2 ;;
  esac
done

if [[ -z "$BRIDGE$APP$PHONE$STATUS$UNINSTALL" ]]; then
  if [[ -t 0 ]]; then
    echo "What do you want to set up on this machine?"
    echo "  1) Bridge     this machine runs herdr and its agents"
    echo "  2) Linux app  the desktop client"
    echo "  3) Phone      install the Android app and pair it with this machine's bridge"
    read -rp "Choose any of 1 2 3 (e.g. 1 3): " choice
    [[ "$choice" == *1* ]] && BRIDGE=1
    [[ "$choice" == *2* ]] && APP=1
    [[ "$choice" == *3* ]] && PHONE=1
    if [[ -n "$BRIDGE" ]] && ! tailscale status >/dev/null 2>&1; then
      read -rp "Tailscale isn't running. Open the bridge to this whole network instead? The token crosses it in clear text. [y/N] " lan
      [[ "$lan" == [yY]* ]] && BIND=0.0.0.0
    fi
  else
    BRIDGE=1
  fi
fi

if [[ -n "$STATUS" ]]; then
  if [[ "$OS" == Darwin ]]; then
    launchctl print "gui/$(id -u)/dev.herdr.bridge" | head -20
    tail -n 30 "$HOME/Library/Logs/herdr-bridge.log"
  else
    systemctl --user status herdr-bridge --no-pager || true
    journalctl --user -u herdr-bridge -n 30 --no-pager
  fi
  tailscale serve status 2>/dev/null || true
fi

# Undoes --bridge and --app; herdr runs nothing on `herdr plugin uninstall`. The token stays, so a phone paired
# with this machine pairs again after a reinstall.
if [[ -n "$UNINSTALL" ]]; then
  if [[ "$OS" == Darwin ]]; then
    launchctl bootout "gui/$(id -u)/dev.herdr.bridge" 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/dev.herdr.bridge.plist"
  elif [[ "$OS" == Linux ]]; then
    systemctl --user disable --now herdr-bridge.service 2>/dev/null || true
    rm -f "$HOME/.config/systemd/user/herdr-bridge.service" "$HOME/.config/systemd/user/herdr-bridge.service.d/lan.conf"
    systemctl --user daemon-reload
  fi
  tailscale serve --tcp "$PORT" off >/dev/null 2>&1 || true
  rm -f "$BIN_DIR/herdr-bridge" "$BIN_DIR/start-bridge.sh"
  rm -rf "$HOME/.local/lib/herdr-mobile" "$BIN_DIR/herdr-mobile" "$HOME/.local/share/applications/herdr-mobile.desktop"
  echo "Removed herdr-bridge and its service, and herdr-mobile's Linux app if it was installed."
  echo "The token stays in ~/.config/herdr-bridge; remove that folder to forget it."
fi

if [[ -n "$BRIDGE" ]]; then
  # A build in this checkout first, then the release's binary, then a build from source (macOS has no release
  # binary, and a download can fail).
  if [[ -f "$SCRIPT_DIR/../target/release/herdr-bridge" ]]; then
    install -m 755 "$SCRIPT_DIR/../target/release/herdr-bridge" "$BIN_DIR/herdr-bridge"
  elif [[ "$OS" == Linux ]] && echo "Downloading herdr-bridge for $ARCH..." &&
    curl -fL --progress-bar -o "$BIN_DIR/herdr-bridge.new" "$BRIDGE_RELEASES/herdr-bridge-linux-$ARCH"; then
    chmod 755 "$BIN_DIR/herdr-bridge.new"
    mv "$BIN_DIR/herdr-bridge.new" "$BIN_DIR/herdr-bridge"
  elif command -v cargo >/dev/null && [[ -f "$SCRIPT_DIR/../Cargo.toml" ]]; then
    rm -f "$BIN_DIR/herdr-bridge.new"
    echo "Building herdr-bridge via cargo..."
    cargo build --release --manifest-path "$SCRIPT_DIR/../Cargo.toml"
    install -m 755 "$SCRIPT_DIR/../target/release/herdr-bridge" "$BIN_DIR/herdr-bridge"
  else
    echo "Error: no herdr-bridge binary for $OS; install Rust (cargo) to build it." >&2
    exit 1
  fi
  install -m 755 "$SCRIPT_DIR/start-bridge.sh" "$BIN_DIR/start-bridge.sh"
  echo "Installed herdr-bridge to $BIN_DIR/herdr-bridge"

  if [[ "$OS" == Darwin ]]; then
    LAUNCH_AGENTS="$HOME/Library/LaunchAgents"
    mkdir -p "$LAUNCH_AGENTS"
    cp "$SCRIPT_DIR/dev.herdr.bridge.plist" "$LAUNCH_AGENTS/dev.herdr.bridge.plist"
    plutil -replace EnvironmentVariables.HERDR_BRIDGE_BIND -string "$BIND" "$LAUNCH_AGENTS/dev.herdr.bridge.plist"
    launchctl bootout "gui/$(id -u)/dev.herdr.bridge" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$LAUNCH_AGENTS/dev.herdr.bridge.plist"
    echo "Started herdr-bridge background service via launchd (dev.herdr.bridge)"
  elif [[ "$OS" == Linux ]]; then
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
  fi

  # Serve keeps this across restarts; on Linux it needs root or `tailscale set --operator=$USER`.
  if tailscale status >/dev/null 2>&1; then
    if [[ "$BIND" == 0.0.0.0 ]]; then
      # The bridge answers on the tailnet itself now.
      tailscale serve --tcp "$PORT" off >/dev/null 2>&1 || true
    elif ! tailscale serve --bg --tcp "$PORT" "tcp://127.0.0.1:$PORT" >/dev/null; then
      echo "Warning: tailscale serve failed. Run: sudo tailscale serve --bg --tcp $PORT tcp://127.0.0.1:$PORT" >&2
    fi
  elif [[ "$BIND" == 127.0.0.1 ]]; then
    echo "Tailscale isn't running, so only this machine can reach the bridge."
    echo "Start Tailscale and run this again, or run it with --lan to open it to this network."
  fi
fi

if [[ -n "$APP" ]]; then
  if [[ "$OS" != Linux ]]; then
    echo "The desktop app is built for Linux only." >&2
  else
    echo "Downloading the herdr-mobile app for $ARCH..."
    tmp="$(mktemp -d)"
    curl -fL --progress-bar "$RELEASES/herdr-mobile-linux-$ARCH.tar.gz" | tar -xz -C "$tmp"
    rm -rf "$HOME/.local/lib/herdr-mobile"
    mkdir -p "$HOME/.local/lib" "$HOME/.local/share/applications"
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
  fi
fi

if [[ -n "$PHONE" ]]; then
  # The address the phone reaches: the tailnet's, else with --lan this network's.
  HOST=
  tailscale status >/dev/null 2>&1 && HOST="$(tailscale ip -4 2>/dev/null | head -n1)"
  if [[ -z "$HOST" && "$BIND" == 0.0.0.0 ]]; then
    if [[ "$OS" == Darwin ]]; then
      HOST="$(ipconfig getifaddr en0 || true)"
    else
      HOST="$(ip -4 route get 1.1.1.1 | awk '{for (i = 1; i < NF; i++) if ($i == "src") print $(i + 1)}')"
    fi
  fi
  # Made on first start if missing; the app sends it with every request.
  TOKEN="$("$BIN_DIR/herdr-bridge" --print-token 2>/dev/null || true)"
  qr() { if command -v qrencode >/dev/null; then qrencode -t ansiutf8 -m 2 "$1"; fi; echo "  $1"; }

  echo ""
  echo "1. Install the app: scan this with the phone's camera, or open the link on the phone."
  qr "$RELEASES/app-release.apk"
  if [[ -z "$HOST" ]]; then
    echo "2. The bridge isn't reachable from the phone: start Tailscale (or set up the bridge with --lan) and run this again." >&2
  elif [[ -z "$TOKEN" ]]; then
    echo "2. The bridge isn't installed here: run this with --bridge first." >&2
  else
    echo ""
    echo "2. Pair: in the app, Machines (top right) ➔ + ➔ Scan QR, or paste the link into Address."
    echo "   It holds this machine's token: don't share it or a screenshot of it."
    qr "herdrmobile://pair?to=$HOST:$PORT&token=${TOKEN//-/}&name=$(hostname -s)"
    echo "   Or type it in: address $HOST:$PORT, token $TOKEN"
    command -v qrencode >/dev/null || echo "   (Install qrencode to show these as QR codes.)"
  fi
fi
