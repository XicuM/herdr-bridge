# herdr-bridge

> **Unofficial.** A community companion for [herdr](https://herdr.dev), not made or endorsed by its authors.

The daemon that lets other devices and apps reach your [herdr](https://herdr.dev) agents. It connects to herdr's local socket, multiplexes remote machines over SSH, and serves herdr's state, terminals, and your Obsidian vaults over authenticated HTTP and WebSockets.

Clients:

- [herdr-mobile](https://github.com/XicuM/herdr-mobile): the Android and Linux app.
- [herdr-obsidian](https://github.com/XicuM/herdr-obsidian): herdr inside Obsidian, desktop and mobile.

The routes and events clients rely on are in [PROTOCOL.md](PROTOCOL.md).

## Install

It installs as a [herdr plugin](https://herdr.dev/docs/plugins/) (herdr 0.9.5 or later, Linux or macOS):

```bash
herdr plugin install XicuM/herdr-bridge     # installs the bridge and starts it
herdr plugin update xicum.herdr-bridge      # updates it
herdr plugin uninstall xicum.herdr-bridge   # the bridge removes itself within a minute
```

Installing puts `herdr-bridge` in `~/.local/bin/`, makes a token at `~/.config/herdr-bridge/token`, and starts a user service (systemd on Linux and WSL2, launchd on macOS). It needs no root. Linux gets the release's binary; macOS, or a failed download, needs [Rust](https://rustup.rs) to build it. The bridge listens on `127.0.0.1:7788`, and the installer brings your tailnet to it with `tailscale serve`.

Uninstalling stops the service and removes the bridge and herdr-mobile's Linux app. The token stays, so a phone you paired pairs again after a reinstall; delete `~/.config/herdr-bridge` to forget it.

The plugin's popups (`herdr plugin pane open --plugin xicum.herdr-bridge --entrypoint <pane>`):

| Pane | What it does |
| --- | --- |
| `pair` | QR codes for the Android app and for pairing it with this machine's bridge (`qrencode` draws them). |
| `app` | Installs herdr-mobile's Linux app. |
| `status` | The service's status, its last log lines and `tailscale serve`. |

Without Tailscale, open the bridge to every network the machine is on with `systemctl --user edit herdr-bridge`, adding `[Service]` and `Environment=HERDR_BRIDGE_BIND=0.0.0.0`, then `herdr plugin update xicum.herdr-bridge`. The token then crosses the network in clear text, so only do this on networks you trust.

```bash
herdr-bridge --print-token                      # the token clients must send
journalctl --user -u herdr-bridge -f            # logs, Linux / WSL2
tail -f ~/Library/Logs/herdr-bridge.log         # logs, macOS
RUST_LOG=herdr_bridge=debug herdr-bridge --port 7788   # foreground, debug logs
```

## Security

- Every request but `/health` needs `Authorization: Bearer <token>`. A wrong token is answered after a second, one at a time, so 8 characters can't be guessed.
- Requests with an `Origin` header are refused, and so is a `Host` that is a dotted name other than `*.ts.net`: no web page can drive the bridge, even through DNS rebinding.
- It listens on `127.0.0.1`, so only this machine and your tailnet (through `tailscale serve`) reach it. Binding `0.0.0.0` opens it to your local network, where the token is sent unencrypted. Direct internet exposure is neither needed nor recommended.

## Obsidian

The bridge serves this machine's Obsidian vaults to the Obsidian plugin on a phone: a phone vault syncs with the vault of the same name in Obsidian's vault list (`~/.config/obsidian/obsidian.json`).

Agents can also show a note to the user:

```bash
herdr-bridge obsidian open notes/Plan.md#Goals --line 12
```

The path is relative to the agent's directory and must be inside a vault Obsidian knows. The bridge tells the Herdr plugin of that vault, which opens it.

## Development

```bash
cargo check
cargo test
cargo build --release
# run a build in place of the installed bridge
install -m 755 target/release/herdr-bridge ~/.local/bin/ && systemctl --user restart herdr-bridge
```

## License

[GPL-3.0-or-later](LICENSE)
