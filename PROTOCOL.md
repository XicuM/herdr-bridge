# Bridge protocol

What clients (herdr-mobile, herdr-obsidian) can rely on. Every request but `/health` carries `Authorization: Bearer <token>` (WebSockets may use `?token=`).

## Addressing

| Prefix | Serves |
| --- | --- |
| `/` | The herdr on the bridge's machine. |
| `/s/<session>/` | That named herdr session, e.g. `herdr-obsidian`. Then the routes below follow. |
| `/m/<id>/` | A machine saved in herdr (`herdr machine list`), reached over SSH. |

## HTTP

| Route | Does |
| --- | --- |
| `GET /health` | `{status, service, version}`. No token needed. |
| `GET /api/auth` | 200 with a valid token, 401 otherwise. |
| `GET /api/snapshot` | Workspaces, tabs, panes, agents. |
| `POST /api/rpc/{method}` | Any herdr API method, JSON params in the body. |
| `POST /api/tab`, `/api/tab/move`, `DELETE /api/tab/{id}`, `POST /api/tab/{id}/rename` | Tabs. |
| `POST /api/pane/clear`, `DELETE /api/pane/{id}` | Panes. |
| `POST /api/workspace`, `/api/workspace/move`, `DELETE /api/workspace/{id}`, `POST /api/workspace/{id}/rename` | Workspaces. |
| `GET/POST /api/worktree`, `POST /api/worktree/open` | Worktrees. |
| `POST /api/uploads` | An image from the clipboard, as base64 JSON. |
| `GET /api/machines`, `POST /api/machines`, `POST/DELETE /api/machines/{id}` | Saved machines. |
| `GET /api/vault/manifest?vault=`, `GET/POST/DELETE /api/vault/file?vault=&path=` | Obsidian vault sync. The manifest lists `{path, mtime, mtime_ms, size}` (`mtime` in seconds, `mtime_ms` in milliseconds); a POST answers the written file's `mtime` and `mtime_ms`. |
| `POST /api/obsidian/open` | `{file, heading?, line?}` (`file` absolute, inside a vault): the Herdr plugin of that vault shows it. 409 when no plugin is connected, 404 outside any vault. |

## WebSockets

- `/ws/session`: JSON messages.
  - `{"type":"snapshot","data":…}` on each change.
  - `{"type":"error","error":"…"}` when there is no snapshot.
  - `{"type":"open","vault":"Notes","path":"a/b.md","heading":"Intro","line":7}` when an agent asks to show a note. Clients of other vaults ignore it.
  - `{"type":"vault_changed","vault":"Notes","files":[{"path":"a/b.md","mtime":…,"mtime_ms":…,"size":…},{"path":"c.md","deleted":true}]}` when synced files of a vault on the bridge's machine change, half a second after a burst of writes ends. Each file is as the manifest would list it now, or deleted. A client that missed some (disconnected, or fell behind) catches up with a full sync. Clients of other vaults ignore it.
- `/ws/term/{pane_id}`: a pane's terminal.

Unknown message types must be ignored by clients, so new ones can be added.
