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

- `placeIds`: the places to sync. Use a dev place, never the live place. A place id belongs to one project. `claim` selects the checkout.
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

- **Models: the side that changed wins.** The daemon records, for each place, the version of each synced `.rbxm` model that Studio and its file last agreed on (`~/.config/studio-sync/agreed/<project>.json`). On a connect it compares both sides with it: if only the file changed (a `git pull` while Studio was closed), Studio loads the file; if only Studio changed (an edit while the daemon was down), Studio's version is saved over the file. If both changed, or nothing is recorded yet, Studio wins and the file goes to `backups/<project>/<time>-replaced/` first. While connected, a model file change loads in Studio, unless Studio has an unsaved edit of it: then Studio's version is saved over the file, after the same backup.
- **Connect makes Studio's code match the disk.** Opening the place, resuming, or restarting the daemon is a full sync. Scripts that differ, and Studio-only instances under the roots, go to `backups/<project>/<time>-studio/`. Then the daemon replaces or removes them.
- **Pause** (the Sync button in the toolbar) stops syncing. Resume is a connect.
- The daemon refuses names that cannot be file names. These are `\ / : * ? " < > |`, a trailing dot or space, `init`, `*.server`, `*.client`, `*.meta`, and names that differ from a sibling only in case. (The file systems of macOS and Windows ignore case.)
- `.toml` and `.csv` files do not sync to Studio (the Rojo build includes them). `status` reports them.

