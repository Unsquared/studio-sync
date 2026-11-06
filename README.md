# studio-sync

Keeps Roblox Studio and your files in sync, so coding agents can work in a Roblox game from its repo. It replaces `rojo serve` and `rojo build`, and uses the same file layout (`default.project.json`), but you don't need Rojo.

- **Code goes from files to Studio** in under a second.
- **Models go both ways:** edit a model in Studio and its `.rbxm` file updates, change the file and Studio updates.
- **Each agent can have its own copy of the place,** so agents in different worktrees don't get in each other's way.
- **Nothing is lost:** whatever a sync overwrites or deletes is backed up first.

If you're an agent, read [`skill/SKILL.md`](skill/SKILL.md). `studio-sync install` also installs it as a Claude skill.

## Install

Once on each machine, with [rokit](https://github.com/rojo-rbx/rokit):

```bash
rokit add --global Unsquared/studio-sync
studio-sync install
```

`install` sets up the Studio plugin, the agent skill and a background daemon that serves all your projects. When Studio asks to allow HTTP for "Studio Sync", allow it. Don't connect the Rojo plugin to a synced place.

To update, run `studio-sync install --update`, then restart Studio. This restarts the daemon, so it refuses while a Studio is connected. `--force` goes ahead, and each Studio does a full sync.

## Set up a project

In the project's folder:

```bash
studio-sync add
```

Then put your dev place id in the `studio-sync.json` it writes:

```json
{
	"placeIds": [1234567890],
	"project": "default.project.json",
	"roots": [["ReplicatedStorage", "shared"], ["ServerScriptService", "server"]]
}
```

`roots` are the paths that sync. To use your own place id without changing it for everyone, put it in `studio-sync.local.json` and keep that file out of git. [`docs/config.md`](docs/config.md) lists every setting.

Use a dev place, never the live place.

## Daily use

1. Open the place with `studio-sync open`, or a private copy with `studio-sync open --copy`.
2. Edit your files.
3. Run `studio-sync wait` before you test. It returns once Studio has your changes, or tells you what's wrong.
4. Run `studio-sync close` when you're done with a copy.

## Commands

| Command | What it does |
|---|---|
| `wait` | Applies file edits to Studio, saves its model edits, and checks what Studio holds. Exit code: 0 in sync, 2 no Studio, 3 errors or differences, 4 timeout |
| `status` | Shows connected Studios, copies and errors |
| `changes [file]` | Shows what happened that git doesn't, in a few short lines. For an `.rbxm`, also what differs from git's version |
| `open [--copy]` | Opens the dev place and makes this checkout the one that syncs it, or opens a private copy |
| `close` | Closes this checkout's copies |
| `pull <path> [file]` | Saves a Studio instance as `.rbxm` |
| `snapshot` | Saves the whole open place, except code, to `place/` |
| `build [-o file]` | Builds a place file from code and the snapshot, by default `build/place.rbxl` |
| `add` | Sets up a project here. Deleting its `studio-sync.json` or its folder unregisters it |
| `install` | Sets up this machine and restarts the daemon. `--update` gets the new version first, `--remove` undoes it |

`studio-sync <command> --help` shows the flags.

## What wins

- **Code: the disk wins.** Script edits made in Studio aren't saved to files. The next sync replaces them, and their text goes to backups.
- **Models: the side that changed wins.** If both changed, Studio wins and the old file goes to backups.
- **Connecting makes Studio's code match the disk.** Under the roots, scripts and instances that exist only in Studio are removed, after a backup.
- **One checkout syncs each place.** If another checkout's Studio is on the place, `open` refuses. `--force` switches that Studio over. To test a worktree, use `open --copy`.

Backups are in `~/.config/studio-sync/backups/` and stay for 30 days.

## Develop

```bash
cargo install --path . && studio-sync install
cargo clippy --all-targets
cargo test
```

A tag `v<version>`, matching the version in `Cargo.toml`, publishes the release binaries. [`CLAUDE.md`](CLAUDE.md) has the contributor rules, and [`docs/protocol.md`](docs/protocol.md) has the HTTP and WebSocket protocol.

## License

[MIT](LICENSE)
