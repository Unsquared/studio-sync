# studio-sync

Two-way sync between Roblox Studio and a Rojo project, for coding agents. It replaces `rojo serve`.

- Files go to Studio in less than one second. Studio edits to scripts go back to the files.
- `snapshot` saves the whole place (maps, UI, lighting) into the repo. `build` makes a place file to publish.
- `open --copy` gives each agent its own private copy of the place.

Agents: read [`skill/SKILL.md`](skill/SKILL.md). It has the full workflow.

## Install

You need Node 23.6+ and `rojo` in each project (rokit). Do this once on each machine:

```bash
git clone git@github.com:Unsquared/studio-sync.git ~/Work/studio-sync
cd ~/Work/studio-sync && npm link && studio-sync install
```

When Studio asks to allow HTTP for "Studio Sync", allow it. Do not connect the Rojo plugin to synced places.

To update, run `git pull && studio-sync install`.

Then, in each project, run `studio-sync add` and put your dev place id in `studio-sync.json`.

## Commands

| | |
|---|---|
| `add` / `remove` / `claim` | register a project / unregister it / make this checkout sync its places (worktrees) |
| `status` / `log` | show what syncs, errors, and the daemon log |
| `wait` | wait until Studio has every file change (exit 0 in sync, 2 no Studio, 3 errors, 4 timeout) |
| `pull <Instance.Path> [file]` | save one Studio instance as `.rbxm` (`--from <session or placeId>` when several Studios are connected) |
| `snapshot` | save the whole open place, except code, into `place/`; `base.rbxl` stays (`--from` as for `pull`) |
| `base <place.rbxl>` | keep the service settings of a real place file (once for each project) |
| `build` | make `build/place.rbxl` from code + snapshot + base |
| `open` / `open --copy` / `close` | open the dev place / open a private copy / close the copies |
| `install` / `restart` / `serve` | machine setup |

## `studio-sync.json`

```json
{
	"placeIds": [1234567890],
	"project": "default.project.json",
	"roots": [["ReplicatedStorage", "shared"], ["ServerScriptService", "server"]]
}
```

`roots` are the paths that sync. `studio-sync.local.json` (not in git) overrides keys for one person, for example their own `placeIds`. All keys and the sync behavior: [`docs/config.md`](docs/config.md).

## Rules

- **Disk wins.** If a script changed on disk and in Studio, the tool keeps the disk version.
- **Connect makes Studio match the disk.** Under the roots, Studio-only scripts and instances are removed.
- **Nothing is lost.** The tool copies all that it overwrites or deletes to `~/.config/studio-sync/backups/`.
- **One syncer for each place.** Use your own dev place, never the live place.

## Develop

Run `npm run check` and `npm test`. [`CLAUDE.md`](CLAUDE.md) has the contributor rules. [`docs/protocol.md`](docs/protocol.md) has the HTTP API. Windows is not tested yet: see the checklist in `CLAUDE.md`.
