---
name: herdr-bridge
description: Send a file to the user's phone (an APK to install, a PDF, an image, a report) or show them a note in Obsidian, through herdr-bridge. Use when the user asks to send, share or put something on their phone or another device, to install a build there, or to show them a file or note. Requires running in a herdr pane (HERDR_ENV=1).
---

# herdr-bridge

The user reaches this machine's herdr agents from herdr-mobile (Android, Linux) and Obsidian through herdr-bridge, which offers agents two commands. Check first that you run inside herdr (`test "${HERDR_ENV:-}" = 1`) and that `herdr-bridge` is on `PATH`; otherwise say so and stop.

## Send a file to herdr-mobile

```bash
herdr-bridge send <path> --title "<one line on what it is>"
```

The user is notified on their phone and opens the file from there: an APK opens Android's installer, anything else the app for its type. The file is copied when sent, so rebuilding afterwards doesn't change what they get. Send finished artifacts only (a built APK, a rendered PDF), up to 200 MB; to send a folder, archive it first.

- Exit 0: sent. Tell the user it's on their phone.
- `No herdr-mobile is connected from another device`: the app isn't open on another device. herdr-mobile running on this same machine doesn't count, since the file is already here: tell the user its path instead.
- Connection refused: herdr-bridge isn't running (`systemctl --user status herdr-bridge`).

## Show a note in Obsidian

```bash
herdr-bridge obsidian open <path>[#heading] [--line N]
```

The path must be inside a vault Obsidian knows. It fails when no Obsidian with the Herdr plugin is connected.
