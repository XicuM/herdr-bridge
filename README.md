# herdr-bridge

> **Unofficial.** A community companion for [herdr](https://herdr.dev), not made or endorsed by its authors.

The daemon that lets other devices and apps reach your [herdr](https://herdr.dev) agents. It connects to herdr's local socket, multiplexes remote machines over SSH, and serves herdr's state, terminals, and your Obsidian vaults over authenticated HTTP and WebSockets.

Clients:

- [herdr-mobile](https://github.com/XicuM/herdr-mobile): the Android and Linux app.
- [herdr-obsidian](https://github.com/XicuM/herdr-obsidian): herdr inside Obsidian, desktop and mobile.

The routes and events clients rely on are in [PROTOCOL.md](PROTOCOL.md).

## Install

Grab `herdr-bridge-linux-{x86_64,aarch64}` from [Releases](https://github.com/XicuM/herdr-bridge/releases), or build it (Rust 2024 edition):

```bash
cargo build --release
./plugin/install.sh             # asks what to set up: the bridge, herdr-mobile's Linux app, or pairing a phone
```

It is also a herdr plugin, whose popups run the installer, pair a phone by QR code and show the bridge's status: `herdr plugin install XicuM/herdr-bridge/plugin`.

The installer puts `herdr-bridge` in `~/.local/bin/`, makes a token at `~/.config/herdr-bridge/token`, and starts a user service (systemd on Linux and WSL2, launchd on macOS). It needs no root. The bridge listens on `127.0.0.1:7788`, and the installer brings your tailnet to it with `tailscale serve`. Without Tailscale, `./plugin/install.sh --bridge --lan` opens it to every network the machine is on instead; the token then crosses the network in clear text, so only do this on networks you trust.

```bash
herdr-bridge --print-token                      # the token clients must send
journalctl --user -u herdr-bridge -f            # logs, Linux / WSL2
tail -f ~/Library/Logs/herdr-bridge.log         # logs, macOS
RUST_LOG=herdr_bridge=debug herdr-bridge --port 7788   # foreground, debug logs
```

## Security

- Every request but `/health` needs `Authorization: Bearer <token>`. A wrong token is answered after a second, one at a time, so 8 characters can't be guessed.
- Requests with an `Origin` header are refused, and so is a `Host` that is a dotted name other than `*.ts.net`: no web page can drive the bridge, even through DNS rebinding.
- It listens on `127.0.0.1`, so only this machine and your tailnet (through `tailscale serve`) reach it. `--lan` opens it to your local network, where the token is sent unencrypted. Direct internet exposure is neither needed nor recommended.

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
```

## License

[GPL-3.0-or-later](LICENSE)
