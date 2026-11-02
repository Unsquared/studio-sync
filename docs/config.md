# Configuration and sync behavior

## `studio-sync.json` (project root)

```json
{
	"placeIds": [1234567890],
	"project": "default.project.json",
	"roots": [["ReplicatedStorage", "shared"], ["ServerScriptService", "server"]],
	"readOnly": [["ReplicatedStorage", "Packages"]],
	"writeback": true,
	"sourcemap": true,
	"content": { "dir": "places/main", "sources": ["src/shared/meshes.luau"] },
	"snapshot": { "dir": "place", "ignore": [["Workspace", "Temp"]] },
	"copy": { "command": ["lune", "run", "build", "dev", "main"], "output": "build/main.rbxl" }
}
```

- `placeIds`: the places to sync. Use a dev place, never the live place. A place id belongs to one project. `claim` selects the checkout.
- `roots`: the DataModel paths to sync. Under a root, Studio matches the disk. On connect, Studio removes what has no file (after a backup). Default: each service, or service child, that the project maps from a `$path`.
- `readOnly`: roots that sync to Studio but never back. Default: the `Packages`-style folders among the roots.
- `writeback`: `false` makes the sync one-way (files -> Studio), like Rojo.
- `sourcemap`: also write `sourcemap.json` when files change.
- `content`: the bake receiver (`POST /content/*`). A Studio script posts `.rbxm` files that replace `dir`. `bake.json` there records the sha256 of each file in `sources`, so a build can refuse a stale bake.
- `snapshot`: `dir` is where `snapshot` saves the place (default `place`). `ignore` lists the paths that `snapshot` leaves out.
- `copy`: for a project that builds its full place itself (its content kept in another format than a snapshot). `open --copy` runs `command` in the project dir, then opens `output` with the copy's session stamped on it. `open --copy --code-only` still builds code only.

`studio-sync.local.json` (add it to `.gitignore`) overrides these keys for one person, usually their own `placeIds`. Config changes apply without a restart.

## How sync behaves

| Studio action | On disk |
|---|---|
| edit a script (editor, MCP, `.Source =`) | the file is rewritten after 0.6 s of quiet |
| new Script / LocalScript / ModuleScript | `name.server.luau` / `name.client.luau` / `name.luau` (a Script with RunContext Client is `.client.luau`) |
| new Folder | a directory (git keeps it after it has a file) |
| a child under a single-file module | `foo.luau` becomes `foo/init.luau` |
| rename or move a script or folder | the file or directory is renamed or moved, with its `.meta.json` |
| delete (or move out of the roots) | the file moves to `~/.config/studio-sync/backups/<project>/<time>-deleted/` |
| any other new instance | nothing, until you `pull` it. `status` lists it as Studio-only. |
| edit a Package, `.json`, `.txt`, `.model.json` or `.rbxm` instance | refused (read-only). Edit the file. |

- **Conflicts: disk wins.** Each Studio edit carries the hash of the text it started from. If the file changed since then, the daemon keeps the file and saves the Studio text to `backups/<project>/<time>-conflict/`.
- **Connect makes Studio match the disk.** Opening the place, resuming, or restarting the daemon is a full sync. Scripts that differ, and Studio-only instances under the roots, go to `backups/<project>/<time>-studio/`. Then the daemon replaces or removes them.
- **Pause** (the Sync button in the toolbar) stops both directions. Resume is a connect.
- The daemon refuses names that cannot be file names. These are `\ / : * ? " < > |`, a trailing dot or space, `init`, `*.server`, `*.client`, `*.meta`, and names that differ from a sibling only in case. (The file systems of macOS and Windows ignore case.)
- Scripts over 900 KB do not sync back (the HTTP limit of Studio). Edit them on disk.
- `.toml` and `.csv` files do not sync to Studio (the Rojo build includes them). `status` reports them.

