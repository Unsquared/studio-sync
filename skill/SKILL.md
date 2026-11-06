---
name: studio-sync
description: Use the studio-sync daemon to keep Roblox Studio and a project's files in sync (code from files to Studio; models both ways). Use if a Roblox repo has a studio-sync.json, before testing code in Studio (Play, MCP execute_luau, specs), when saving models/maps (things built in Studio) into the repo, when Studio runs stale code, when saving the whole place / snapshot / build / publish a place file, when an agent needs a private copy of the place, or when setting up a new project or worktree. Replaces `rojo serve`, `rojo build` and the Connect button of the Rojo plugin.
---

# studio-sync

An always-on daemon (localhost:34999) and a Studio plugin. A place that a project lists in `studio-sync.json` syncs when it is open in **Edit**:

- **files -> Studio**, within one second: scripts, folders, `.model.json` / `.meta.json` properties and attributes, `.json` / `.txt` files, `.rbxm` models.
- **Studio -> files**, live: edits to a synced `.rbxm` model (move a part, add or delete children, change properties or attributes) save over its file after one second of quiet.
- **Studio -> files on request**: any other instance (a model, a part, a map) with `studio-sync pull`. After that it is a synced model.
- **Scripts go one way.** Edit them on disk. A script edited in Studio (editor or MCP) is not saved: the next change of its file, or the next connect, replaces it (the Studio text goes to backups).

There is no Connect button and no `rojo serve`. Do not start them. `build` needs no Rojo either.

## The loop for a code change

1. Edit the files on disk. Script edits made in Studio are not saved.
2. Run `studio-sync wait`. It settles both directions: Studio's model edits are saved to their files, and Studio applies every file change.
   - Exit 0: in sync. Test now.
   - Exit 2: no Studio connected: the dev place isn't open in Edit, the plugin isn't loaded, or the place id isn't in `studio-sync.json`. Tell the user. Don't test against stale code.
   - Exit 3: Studio reported errors, the last sync failed, Studio lacks something even after a full sync, or a script's text in Studio differs from its file. The message says which. Put wanted changes in the files, then run `studio-sync wait --full`.
   - Exit 4: timeout. Run `studio-sync wait --full` once. If that times out too, tell the user: Studio may be hung or showing a dialog.
3. Test in Studio (Play, MCP). **A running Play keeps the code it started with**: restart it after a sync. `wait` says so when it sees an older Play, and still exits 0. Edit mode caches `require`, so a module that Edit required (command bar, MCP `execute_luau`) keeps its old code for the session. Test in Play, or use `loadstring(module.Source)`.

Run `studio-sync wait` after **each** batch of edits you want to test. Chain it to the edit (`... && studio-sync wait`) so it costs no turn of its own. It also replaces a `sleep` after a model edit in Studio: it prints `saved from Studio: <file>` for each file saved since the last `wait`. If you didn't expect one, run `studio-sync changes <file>` before you commit.

`wait` puts back anything removed, renamed or moved in Studio. If Studio still seems wrong, or a touched file doesn't arrive (the same content is no change), run `studio-sync wait --full`. Don't edit a file to force a re-send.

## Rules that keep work safe

- **Models: the side that changed wins; code: disk wins.** On a connect, each synced `.rbxm` model is compared with the version Studio and its file last agreed on. A file changed by a `git pull` while Studio was closed loads in Studio; a model edited in Studio while the daemon was down is saved over its file. If both changed, Studio wins and the file goes to `~/.config/studio-sync/backups/<project>/<time>-replaced/`. While connected, a changed model file loads in Studio, unless Studio has an unsaved edit of that model (then Studio wins, with the same backup). The Studio Output and `studio-sync changes` show the paths.
- **A connect makes Studio's code match the disk.** A connect occurs when you open the place, resume, or restart the daemon. Studio replaces scripts that differ, and instances under the roots that have no file. The Studio copies go to `backups/<project>/<time>-studio/`. WARNING: `pull` anything else that you made in Studio under a root before the next connect. If not, the connect removes it.
- **Any edit of a synced model in Edit saves to its file**, also from a test or an MCP script: properties, attributes, tags, children. Do throwaway experiments on a copy of the model outside the roots, or `git checkout` the file after. `git status` shows what was saved.
- **Nothing but model edits saves from Studio.** New scripts, folders, renames, moves and deletes in Studio stay in Studio, and the next connect undoes them. Do them on disk.
- **Read-only in Studio:** everything except `.rbxm` models outside Packages and `readOnly`. `.rbxmx` models are read-only too (Studio saves binary only).
- Only the **roots** in `studio-sync.json` sync. Save everything else in the place (Workspace, Lighting, properties of services) with `snapshot` (the whole place) or `pull` (one instance).

## Save things built in Studio

```bash
studio-sync pull ReplicatedStorage.shared.assets.Tree          # under a root: next to its siblings' files, as Tree.rbxm
studio-sync pull Workspace.Map assets/map.rbxm                 # anywhere else: name the file
studio-sync pull 'Workspace["Odd.Name"]' assets/odd.rbxm       # names with dots go in brackets
```

A pulled instance under a root becomes a synced model. The "Save to disk" button in the Studio toolbar does the same for the selection.

If several Studios are connected to the project (the dev place and copies), `pull` and `snapshot` refuse until you name one: `--from <session>` for a copy, `--from <placeId>` for the place. `studio-sync status` lists them.

## The whole place: snapshot and build

```bash
studio-sync snapshot     # everything in the open Studio the code files don't own -> place/ (commit it)
studio-sync build        # build/place.rbxl = code + place/
```

`snapshot` reads Studio directly: no saved version, no API key. It replaces the snapshot dir. Before you commit, review with `git status` and `git diff --stat place/`. The files are binary `.rbxm`, one for each top-level instance.

A plugin can't read some service settings (`Lighting.Technology`, `Workspace.SignalBehavior`...), so a built place gets Roblox defaults for them unless `$properties` in the project file sets them. See `config.md`.

## Your own Studio: copies

```bash
studio-sync open --copy              # build from this checkout (or with the project's `copy` command), open in a new Studio (background), wait until synced. It leaves model files as they are.
studio-sync open --copy --code-only  # faster, no snapshot: code only
studio-sync close                    # done: closes this checkout's copy, no save (snapshot first if it has non-code changes). A copy you closed by hand goes to backups by itself.
studio-sync open                     # the project's published dev place instead
```

A copy syncs with the checkout that made it, and with nothing else. Agents in different worktrees do not touch the dev place of anyone.

Several Studios are then open. With the Studio MCP, `list_roblox_studios` shows the copy as `<checkout>-copy-<session>.rbxl`. Use that id. Changes to non-code content in a copy reach the repo only through `studio-sync snapshot --from <session>` or `pull ... --from <session>`. You cannot publish a copy. Run `build` and publish the file.

## Status and fixes

```bash
studio-sync status          # daemon, this project (outside a project: every project): roots, Studios, copies, errors
studio-sync install         # sets up this machine again and restarts the daemon
```

- The Studio Output says "no project for place ...", or `status` says "Studio: not connected". The place id is not listed. Add it to `placeIds` in `studio-sync.json`, or to `studio-sync.local.json` (personal, untracked) for your own dev place.
- The message says "place ... is listed by A and B". Two checkouts (a worktree) list the same place. Run `studio-sync open` in the checkout to sync: it takes the place, and the Studio resyncs within seconds. It refuses if a Studio on the place syncs with another checkout (`--force` switches it). Someone else may be using that Studio; to test a worktree, use `open --copy`.
- The Studio Output has no `[studio-sync]` lines. The plugin is not loaded (restart Studio; `studio-sync install` rewrites it), or its HTTP permission was denied (Plugins > Manage Plugins > Studio Sync).
- The Sync button in the toolbar pauses the sync. While it is paused, nothing syncs in either direction. A resume makes Studio match the disk again.
- The daemon does not run. Run `studio-sync install` (once for each machine) to start it again.

## Set up a project

```bash
studio-sync add            # in the project root: writes studio-sync.json with default roots, registers it
```

Then set `placeIds` to a **dev place**. Never use the live place. Each person must sync their own dev place. Put it in `studio-sync.local.json` and add that file to `.gitignore`. If two people sync the same Team Create place, the syncs fight. The config reference is `config.md` next to this file.

## When something changed and you didn't do it

Run `studio-sync changes`. In a few short lines it says what happened in this project in the last hour that your edits and git don't: models saved from Studio, things gone from Studio, backups (under `~/.config/studio-sync/`). Use `--since 1d` to look further back. `studio-sync changes <file.rbxm>` says what differs in a model from git's version of it (`git diff` only says "Binary files differ"; don't dump the model, a map runs to megabytes): how many instances lost a tag, moved, were added. `--rev HEAD~1` compares with another version, and `-n 100` gives more lines and full instance paths. `--raw` gives the raw events, for debugging the tool: pipe it through `grep`, it is long.
