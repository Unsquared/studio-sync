# studio-sync

Sync between Roblox Studio and a Rojo project, for coding agents. It replaces `rojo serve`.

- Files go to Studio in less than one second. Code is edited on disk; models (`.rbxm`) edited in Studio save back to their files.
- `snapshot` saves the whole place (maps, UI, lighting) into the repo. `build` makes a place file to publish.
- `open --copy` gives each agent its own private copy of the place.

Agents: read [`skill/SKILL.md`](skill/SKILL.md). It has the full workflow.

## Install

With [rokit](https://github.com/rojo-rbx/rokit), once on each machine:

```bash
rokit add --global Unsquared/studio-sync
studio-sync install
```

`install` starts the background daemon (one for all projects, so install it globally, not per repo) and writes the Studio plugin and the agent skill, which the binary carries. Live sync reads the Rojo project file itself; only `build` and `open --copy` run `rojo` (rokit, in the project).

When Studio asks to allow HTTP for "Studio Sync", allow it. Do not connect the Rojo plugin to synced places.

To update, run `rokit update --global studio-sync && studio-sync install`, then restart Studio.

Then, in each project, run `studio-sync add` and put your dev place id in `studio-sync.json`.

## Commands

| | |
|---|---|
| `add` / `remove` / `claim` | register a project / unregister it / make this checkout sync its places (worktrees) |
| `status` / `log` | show what syncs, errors, and the daemon log |
| `wait` | wait until Studio has every file change (exit 0 in sync, 2 no Studio, 3 errors, 4 timeout) |
| `pull <Instance.Path> [file]` | save one Studio instance as `.rbxm` (`--from <session or placeId>` when several Studios are connected) |
| `snapshot` | save the whole open place, except code, into `place/` (`--from` as for `pull`) |
| `build` | make `build/place.rbxl` from code + snapshot |
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

- **Disk wins for code; for models, the side that changed wins.** Script edits in Studio are not saved. On a connect, a model file changed by a `git pull` loads in Studio, and a model edited in Studio is saved over its file. If both changed, Studio wins and the file goes to backups.
- **Connect makes Studio's code match the disk.** Under the roots, Studio-only scripts and instances are removed.
- **Nothing is lost.** The tool copies all that it overwrites or deletes to `~/.config/studio-sync/backups/`.
- **One syncer for each place.** Use your own dev place, never the live place.

## Develop

From a checkout: `cargo install --path . && studio-sync install` runs your build. Run `cargo clippy --all-targets` and `cargo test`. A tag `v<version>` (the version in Cargo.toml) publishes the release binaries. [`CLAUDE.md`](CLAUDE.md) has the contributor rules. [`docs/protocol.md`](docs/protocol.md) has the HTTP and WebSocket protocol. Windows is not tested yet: see the checklist in `CLAUDE.md`.
