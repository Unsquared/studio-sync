# HTTP protocol

All routes are on `http://localhost:34999`. A request with an `Origin` header, or with a `Host` that is not localhost, gets 403.

Studio requests carry `placeId`. Plugin requests also carry `client` (a GUID for each plugin session) and `plugin` (its version). In a copy, they also carry `session`. The daemon finds the project by `session` (the checkout that made the copy). Else it uses the place id: the project that `claim`ed it, else the one project that lists it (409 if two list it, 404 if none).

## Studio -> daemon

| | |
|---|---|
| `GET /tree` | the full tree: `{ name, epoch, version, roots, writeback, nodes: Node[] }` |
| `GET /changes?epoch&since&wait` | a long-poll (up to 30 s) for versions after `since`: `{ full: false, version, upserts: Node[], removes: path[], commands }`. If the epoch changed, or `since` is older than the 300 kept deltas, it returns `{ full: true }`. `since` is also the acknowledgement of the client for what it applied. |
| `GET /model?file` | a model file of the project, base64 |
| `POST /writeback` | `{ ops: Op[] }` -> `{ results: Result[] }`, in order |
| `POST /backup` | `{ items: { path, source? , rbxm? }[] }` -> `{ dir }`: the Studio side of what a full sync replaces |
| `POST /report` | `{ version, errors, unsaved }`: the apply errors and Studio-only instances. `status` and `wait` show them. |
| `POST /pull?upload&id&path&file&part&last` | a serialized instance, base64, in parts under 1 MB. The last part (`last=1`) saves it -> `{ ok, file }`. `?id&error=` reports a failed pull command. |
| `POST /content/begin`, `/content/file/<Service>/<Name>.rbxm?part`, `/content/commit` | the bake receiver (`content` in studio-sync.json) |
| `GET /snapshot/plan` | `{ tree, ignore }`: the tree of the project file. Each node is `{ owned, children }`. If `owned` is true, the files own the node: leave it out. |
| `POST /snapshot/file?token&path&last` | the `.rbxm` of one top-level instance, base64, in parts under 1 MB |
| `POST /snapshot/commit?token` | `{ placeId, entries: { path, className }[], services, skipped, duplicates }`: replaces the snapshot dir (the old one goes to a backup) |
| `POST /command/result` | `{ id, ok, error?, ... }`: the result of a queued command |
| `GET /plugin/sync.luau` | the plugin module, for `loadstring` |
| `GET /tools/<file>.luau` | a file under `tools/` of the project |

`Node`: `{ path: string[], className, kind, ro?, source?, sourceHash?, runContext?, properties?, attributes?, model?, modelHash?, error?, hash }`.

`kind` is `script` (a .luau file), `init` (the init script of a directory), `dir`, `generated` (.json / .txt / .toml / .csv), `model` (.rbxm / .rbxmx), `json` (.model.json), or `project` (declared by the project file). Only `script`, `init`, and `dir` take writeback, and not if `ro` is set.

`Op`: `{ op: "source", path, source, base }` (`base` is the `sourceHash` where the edit started), `{ op: "create", path, className, source?, runContext? }`, `{ op: "delete", path }`, `{ op: "move", from, to }`.

`Result`: `{ ok, error?, sourceHash?, conflict?: { source, sourceHash, saved } }`.

## CLI -> daemon

| | |
|---|---|
| `GET /status` | every project, its clients, and its errors |
| `GET /wait?dir&timeout` | regenerates now, then waits until every live client acknowledges the current version: `{ ok, reason?: "no-studio" \| "timeout", version, error, clients }` |
| `POST /pull/request?dir&path&file&from` | queues a pull command for a Studio of the project and waits (60 s) for the result |
| `POST /snapshot/request?dir&from` | queues a snapshot command for a Studio of the project and waits (600 s) |

Commands (`pull`, `snapshot`) go in the `commands` field of `/changes` responses. The daemon sends them to the client that `from` names (a copy's `session`, or the `placeId` of a client that is not a copy), else to the only live client. With several live clients and no `from`, or a `from` that matches no client or more than one, the request gets 409 with the connected clients in the message.
