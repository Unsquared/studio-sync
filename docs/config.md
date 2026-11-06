# Configuration and sync behavior

## `studio-sync.json` (project root)

```json
{
	"placeIds": [1234567890],
	"project": "default.project.json",
	"roots": [["ReplicatedStorage", "shared"], ["ServerScriptService", "server"]],
	"readOnly": [["ReplicatedStorage", "Packages"]],
	"writeback": true,
	"content": { "dir": "places/main", "sources": ["src/shared/meshes.luau"] },
	"snapshot": { "dir": "place", "ignore": [["Workspace", "Temp"]] },
	"copy": { "command": ["lune", "run", "build", "dev", "main"], "output": "build/main.rbxl" }
}
```

- `placeIds`: the places to sync. Use a dev place, never the live place. A place id belongs to one project. `studio-sync open` in a checkout makes it the one that syncs.
- `roots`: the DataModel paths to sync. Under a root, Studio matches the disk. On connect, Studio removes what has no file (after a backup). Default: each service, or service child, that the project maps from a `$path`.
- `readOnly`: roots that sync to Studio but never back. Default: the `Packages`-style folders among the roots.
- `writeback`: `false` stops Studio model edits from saving to their files (files -> Studio only, like Rojo).
- `content`: the bake receiver (`POST /content/*`). A Studio script posts `.rbxm` files that replace `dir`. `bake.json` there records the sha256 of each file in `sources`, so a build can refuse a stale bake.
- `snapshot`: `dir` is where `snapshot` saves the place (default `place`). `ignore` lists the paths that `snapshot` leaves out.
- `copy`: for a project that builds its full place itself (its content kept in another format than a snapshot). `open --copy` runs `command` in the project dir, then opens a copy of `output` named for the copy's session. `open --copy --code-only` still builds code only.

`studio-sync.local.json` (add it to `.gitignore`) overrides these keys for one person, usually their own `placeIds`. Config changes apply without a restart.

## How sync behaves

| Studio action | On disk |
|---|---|
| edit a `.rbxm` model (properties, attributes, children) | the file is rewritten after 1 s of quiet (and on the next connect, if the edit was made offline) |
| edit a script (editor, MCP, `.Source =`) | nothing. The editor warns once; the next change of the file replaces it, and its text goes to backups. |
| new script, Folder or other instance | nothing, until you `pull` it (then it is a `.rbxm` model). The next connect removes it, after a backup. |
| rename, move or delete | nothing; the next connect restores it |
| edit a Package, `.rbxmx`, `.json`, `.txt` or `.model.json` instance | nothing (read-only). Edit the file. |

- **Models: the side that changed wins.** The daemon records, for each place, the version of each synced `.rbxm` model that Studio and its file last agreed on (`~/.config/studio-sync/agreed/<project>.json`). On a connect it compares both sides with it: if only the file changed (a `git pull` while Studio was closed), Studio loads the file; if only Studio changed (an edit while the daemon was down), Studio's version is saved over the file. If both changed, or nothing is recorded yet, Studio wins and the file goes to `backups/<project>/<time>-replaced/` first. A new copy is the exception: it was built from the files and holds no work, so its first connect writes nothing, and loads only a file changed since the build. While connected, any edit of a model in Studio saves to its file, tags included, and a model file change loads in Studio, unless Studio has an unsaved edit of it: then Studio's version is saved over the file, after the same backup.
- **Connect makes Studio's code match the disk.** Opening the place, resuming, restarting the daemon, or `studio-sync wait --full` is a full sync. Scripts that differ, and Studio-only instances under the roots, go to `backups/<project>/<time>-studio/`. Then the daemon replaces or removes them. A module generated from a `.json` or `.txt` file is replaced without a backup: it holds no work, and Rojo words it differently from the sync, so every built copy differs.
- **`wait` checks what Studio holds.** After Studio applies the last version, it looks up each synced instance at its path and compares each script's text with its file. Something gone, renamed or moved means one full sync. Text that differs is left alone and `wait` exits 3 until the file or Studio changes (`studio-sync resync` replaces it). No file event tells of an undo or a script in Studio: only this check does, so `status` shows what the last `wait` found.
- **Backups stay 30 days.** When they pass 1 GB together, the oldest go first, but none younger than 7 days. The daemon checks when it starts and at most hourly after a backup.
- **Pause** (the Sync button in the toolbar) stops syncing. Resume is a connect.
- The daemon refuses names that cannot be file names. These are `\ / : * ? " < > |`, a trailing dot or space, `init`, `*.server`, `*.client`, `*.meta`, and names that differ from a sibling only in case. (The file systems of macOS and Windows ignore case.)
- `.toml` and `.csv` files do not sync to Studio (`build` includes them). `status` reports them.

## Logs

| file | what |
| --- | --- |
| `~/Library/Logs/studio-sync.log` (macOS), `~/.config/studio-sync/daemon.log` (Windows) | the daemon's log, for people. Two files of 5 MB. |
| `~/.config/studio-sync/events.jsonl` | what happened, as JSON lines, for analysis: `studio-sync changes --raw`. Always on, stays on this machine, two files of 20 MB. |

`studio-sync changes` is the short form for agents: this project only, one line per burst, at most 10 lines of 200 characters (under 600 tokens at worst, usually far less). `changes <file.rbxm>` also compares the model with git's version of it, grouped by what changed: at most 12 kinds of change. It reads the file with `rbx_binary`; ids that change on each save are ignored, and a property only one version's file has is counted but not listed, since a newer Studio writes properties an older one didn't. This output stays in an agent's context on every later turn, so keep these bounds when you change it.

Each event has `t` (UTC) and `ev`:

| `ev` | fields |
| --- | --- |
| `cli` | a command: `args`, `cwd`, `ms`, `code`, `message` (its failure), `version` |
| `daemon` | the daemon started: `version`, `pid` |
| `regen` | a new version: `project`, `version`, `ms`, `upserts`, `removes` (each `{ count, first }`: the first 20 names) |
| `connect`, `disconnect` | a Studio's socket: `project`, `client`, `who`, `plugin`, `version` |
| `studio` | from the plugin. `name: "synced"`: a connect's full sync, with `seconds`, `result`, `lastDisconnect` (why it had lost the connection) |
| `applied` | a Studio applied a `version` |
| `wait` | a `wait` ended: `ms`, `ok`, `reason`, `version`, `clients`, `saved`, `plays`, `checks` |
| `drift` | what a `wait` found in a Studio: `missing`, `edited` |
| `resync` | `studio-sync wait --full` |
| `errors` | what a Studio reports as wrong changed: `errors` |
| `model` | a model saved or loaded: `place`, `mode`, `path`, `bytes`, `action`, `replaced`, `error` |
| `backup`, `prune` | Studio items backed up: `items`, `dir`; old backups removed: `removed`, `bytes` |
| `play` | a Play started or stopped: `who`, `version`, `running` |

## Service settings a build needs

A plugin can't read some service settings: `Workspace.SignalBehavior`, `ExplicitAutoJoints`, `PhysicsSteppingMethod`, `ModelStreamingBehavior`, `Lighting.Technology`. The snapshot leaves them out, so the built place gets Roblox defaults for them unless the project file sets them. Set them once as `$properties` on the service in `default.project.json` (they win over the snapshot), with the values Studio's Properties window shows for the dev place:

```json
"Workspace": { "$properties": { "SignalBehavior": "Deferred", "StreamingEnabled": true } },
"Lighting": { "$properties": { "Technology": "Future" } }
```
