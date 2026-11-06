# Protocol

All routes are on `http://localhost:34999`. A request with an `Origin` header, or with a `Host` that is not localhost, gets 403.

Studio requests carry `placeId`. Plugin requests also carry `client` (a GUID for each plugin session) and `plugin` (its version). In a copy, they also carry `session`, which the plugin reads from the copy's file name (`<checkout>-copy-<session>.rbxl`, its `game.Name`). The daemon finds the project by `session` (the checkout that made the copy). Else it uses the place id: the project that `claim`ed it, else the one project that lists it (409 if two list it, 404 if none).

## The socket

After a `GET /tree`, the plugin opens a WebSocket: `GET /socket?placeId&session&client&plugin&epoch&version`, where `epoch` and `version` are the tree's. The Studio is connected exactly while it is open: `status`, `wait` and commands see it at once, and when it closes, the commands it never answered fail at once. The messages are small JSON texts; trees, deltas and uploads stay on HTTP.

| daemon -> Studio | |
|---|---|
| `{ type: "version", epoch, version }` | a new version: read `GET /changes` |
| `{ type: "pull", id, path, file? }` | save the instance at `path` (`POST /pull`) |
| `{ type: "snapshot", id }` | save the whole place (`/snapshot/*`) |
| `{ type: "flush", seq }` | save every edited model now, without the quiet time (`POST /save`), retry nodes whose apply failed, then answer `flushed` |
| `{ type: "verify", seq }` | check that Studio still holds what the sync gave it, then answer `verified` |

| Studio -> daemon | |
|---|---|
| `{ type: "applied", version }` | the version Studio has applied; `wait` waits for it |
| `{ type: "report", errors }` | all that is wrong now. A node's error stays until that node next applies. `status` and `wait` show them. |
| `{ type: "flushed", seq, missing }` | the flush is done; `wait` waits for it (at most 10 s, and only from a plugin of the daemon's version). |
| `{ type: "verified", seq, missing, edited }` | the answer to `verify`, with at most 20 names each. `missing`: instances the sync made that are no longer at their path; the plugin is doing a full sync and sends `applied` after it. `edited`: scripts whose text differs from what the sync gave them. |
| `{ type: "event", name, detail }` | goes to `events.jsonl`. `name: "synced"`: a connect's full sync. |

## The Play socket

When `open` moves a place to another checkout, the daemon closes the socket of the Studio on it. The plugin connects again and finds the new checkout.

A Play client opens `GET /play?placeId&session&client` and says nothing more. A Play runs the version that was current when it opened; `status` and `wait` name it until it closes.

## Studio -> daemon

| | |
|---|---|
| `GET /tree` | the full tree: `{ name, epoch, version, roots, writeback, nodes: Node[] }` |
| `GET /changes?epoch&since` | the changes after `since`, merged: `{ full: false, version, upserts: Node[], removes: path[] }`. If the epoch changed, or `since` is older than the 300 kept deltas, it returns `{ full: true }`: read `/tree` again. |
| `GET /model?file` | a model file of the project, base64 |
| `POST /save?upload&mode&base&path&part&last` | a synced `.rbxm` model from Studio, base64, in parts under 1 MB -> `{ ok: true, action, modelHash, file, replaced? }` or `{ ok: false, error }`. `mode=edit`: an edit; saved over the file, which is backed up first if it is not `base` (the file hash Studio's version came from). `mode=connect`: compared with the version Studio and the file last agreed on (per place); `action` is `load` (only the file changed: load it), `saved` (Studio's version saved; `replaced` names the backup when the file changed too), or `none`. `mode=loaded`: Studio loaded the file whose hash is `base`; its serialization is recorded (`action: recorded`). |
| `POST /backup` | `{ items: { path, source? , rbxm? }[] }` -> `{ dir }`: the Studio side of what a full sync replaces |
| `POST /pull?upload&id&path&file&part&last` | a serialized instance, base64, in parts under 1 MB. The last part (`last=1`) saves it -> `{ ok, file }`. `?id&error=` reports a failed pull command. |
| `POST /content/begin`, `/content/file/<Service>/<Name>.rbxm?part`, `/content/commit` | the bake receiver (`content` in studio-sync.json) |
| `GET /snapshot/plan` | `{ tree, ignore }`: the tree of the project file. Each node is `{ owned, children }`. If `owned` is true, the files own the node: leave it out. |
| `POST /snapshot/file?token&path&last` | the `.rbxm` of one top-level instance, base64, in parts under 1 MB |
| `POST /snapshot/commit?token` | `{ placeId, entries: { path, className }[], services, skipped, duplicates }`: replaces the snapshot dir (the old one goes to a backup) |
| `POST /command/result?id&error` | the result of a command |
| `GET /plugin/sync.luau` | the plugin module, for `loadstring` |
| `GET /tools/<file>.luau` | a file under `tools/` of the project |

`Node`: `{ path: string[], className, kind, ro?, source?, sourceHash?, runContext?, properties?, attributes?, model?, modelHash?, error?, hash }`.

`kind` is `script` (a .luau file), `init` (the init script of a directory), `dir`, `generated` (.json / .txt / .toml / .csv), `model` (.rbxm / .rbxmx), `json` (.model.json), or `project` (declared by the project file). Only a `model` from a `.rbxm` file saves back from Studio, and not if `ro` is set.

## CLI -> daemon

| | |
|---|---|
| `GET /status` | every project, its clients, and its errors |
| `POST /resync?dir` | every client does a full sync: a new epoch and version, so its next `GET /changes` answers `{ full: true }` |
| `GET /wait?dir&timeout` | asks each live client to save its model edits (`flush`), regenerates now, then waits until every live client acknowledges the current version (with no client and none fetching `/tree` in the last 60 s, it answers `no-studio` after 3 s), then asks each to `verify`, and once more after a full sync: `{ ok, reason?: "no-studio" \| "timeout", version, error, clients, saved, plays }`. `saved`: the model files saved from Studio since the last `wait`. `plays`: `{ who, version, stale }[]`. |
| `POST /pull/request?dir&path&file&from` | queues a pull command for a Studio of the project and waits (60 s) for the result |
| `POST /snapshot/request?dir&from` | queues a snapshot command for a Studio of the project and waits (600 s) |

Commands (`pull`, `snapshot`) go down the socket. The daemon sends them to the client that `from` names (a copy's `session`, or the `placeId` of a client that is not a copy), else to the only live client. With several live clients and no `from`, or a `from` that matches no client or more than one, the request gets 409 with the connected clients in the message.
