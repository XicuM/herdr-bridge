# Herdr Bridge

[![herdr 0.9.5+](https://img.shields.io/badge/herdr-0.9.5%2B-4B5563)](https://herdr.dev)
[![Linux and macOS](https://img.shields.io/badge/platform-linux%20%7C%20macos-4B5563)](#requirements)
[![License: GPL v3](https://img.shields.io/badge/license-GPLv3-22C55E)](https://www.gnu.org/licenses/gpl-3.0)

> **Unofficial plugin.** A community plugin for [herdr](https://herdr.dev), not made, endorsed or supported by its authors.

**Your herdr agents, from your phone and Obsidian.** This plugin sets up [herdr-bridge](https://github.com/XicuM/herdr-bridge), the daemon that serves herdr's agents, terminals and Obsidian vaults to [herdr-mobile](https://github.com/XicuM/herdr-mobile) (Android and Linux) and [herdr-obsidian](https://github.com/XicuM/herdr-obsidian), over your tailnet and behind a token.

## Install

```bash
herdr plugin install XicuM/herdr-bridge/plugin
```

## Panes

| Pane | What it does |
| --- | --- |
| **Set up herdr-bridge** | Asks what to set up: the bridge (installs `herdr-bridge` to `~/.local/bin` and starts it as a user service, systemd or launchd), herdr-mobile's Linux app, or a phone. |
| **Pair phone** | Shows QR codes for the Android app and for pairing it with this machine's bridge. |
| **Bridge status** | The service's status, its last log lines and `tailscale serve`. |

Open one with `herdr plugin pane open --plugin xicum.herdr-bridge --entrypoint setup` (or `pair`, `status`), or bind it to a key in `~/.config/herdr/config.toml`.

## Requirements

- herdr 0.9.5 or later, on Linux or macOS. No root needed.
- [Tailscale](https://tailscale.com) to reach the bridge from other devices. Without it, setup can open the bridge to your local network instead, where the token crosses in clear text.
- On macOS, or when the release download fails, [Rust](https://rustup.rs) to build the bridge.
- Optional: `qrencode`, to show the pairing links as QR codes.

## Security

The bridge listens on `127.0.0.1:7788` and `tailscale serve` brings your tailnet to it. Every request needs the token in `~/.config/herdr-bridge/token`: the pairing QR code holds it, so don't share it or a screenshot of it. See the [bridge's README](../README.md#security).

## License

[GPL-3.0-or-later](LICENSE)
