---
name: studio-sync
description: Use the studio-sync daemon to keep Roblox Studio and the files of a Rojo project in sync in both directions. Use if a Roblox repo has a studio-sync.json, before testing code in Studio (Play, MCP execute_luau, specs), after MCP edits of scripts in Studio, when saving models/maps (things built in Studio) into the repo, when Studio runs stale code, when saving the whole place / snapshot / build / publish a place file, when an agent needs a private copy of the place, or when setting up a new project or worktree. Replaces `rojo serve` and the Connect button of the Rojo plugin.
---

# studio-sync

An always-on daemon (localhost:34999) and a Studio plugin. A place that a project lists in `studio-sync.json` syncs when it is open in **Edit**:

- **files -> Studio**, within one second: scripts, folders, `.model.json` / `.meta.json` properties and attributes, `.json` / `.txt` files, `.rbxm` models.
- **Studio -> files**, live, under the synced roots: script Source (typed in the editor or set through an MCP), new scripts and folders, renames, moves, and deletes.
- **Studio -> files on request**: any other instance (a model, a part, a map) with `studio-sync pull`.

There is no Connect button and no `rojo serve`. Do not start them.

## The loop for a code change

1. Edit the files on disk (preferred), or edit in Studio through the MCP. Both end up in the repo.
2. Run `studio-sync wait`. It waits until Studio applies every file change.
   - Exit 0: in sync. Test now.
   - Exit 2: no Studio is connected. The dev place is not open in Edit, the plugin is not loaded, or the place id is not in `studio-sync.json`. Tell the user. Do not test against stale code.
   - Exit 3: Studio applied the changes but reported errors (the command prints them), or the last sync failed (a bad project file, a rojo error).
   - Exit 4: timeout. Check `studio-sync status`.
3. Test in Studio (Play, MCP). Edit mode caches `require`. A ModuleScript that Edit required (command bar, MCP `execute_luau` in Edit) keeps its old code for the session. Test in Play, or load fresh source with `loadstring(module.Source)`. Do not trust a second `require` in Edit.

Run `studio-sync wait` after **each** batch of file edits that you want to test. It is cheap if nothing changed.

## Rules that keep work safe

- **Disk wins conflicts.** If a script changed on disk and in Studio before the two met, the disk version stays. The Studio text goes to `~/.config/studio-sync/backups/<project>/<time>-conflict/`. The Studio Output and `studio-sync log` show the path.
- **A connect makes Studio match the disk.** A connect occurs when you open the place, resume, or restart the daemon. Studio replaces scripts that differ, and instances under the roots that have no file. The Studio copies go to `backups/<project>/<time>-studio/`. WARNING: `pull` anything else that you made in Studio under a root before the next connect. If not, the connect removes it.
- **A delete in Studio deletes the file.** A copy goes to `backups/<project>/<time>-deleted/`. A move of a script out of the synced roots is a delete.
- **Read-only in Studio:** Packages (and anything in `readOnly`), `.json` / `.txt` modules, `.model.json` instances, `.rbxm` models. Edit their files. Studio does not save edits to them and replaces them.
- Only the **roots** in `studio-sync.json` sync. Save everything else in the place (Workspace, Lighting, properties of services) with `snapshot` (the whole place) or `pull` (one instance).

## Save things built in Studio

```bash
studio-sync pull ReplicatedStorage.shared.assets.Tree          # under a root: next to its siblings' files, as Tree.rbxm
studio-sync pull Workspace.Map assets/map.rbxm                 # anywhere else: name the file
studio-sync pull 'Workspace["Odd.Name"]' assets/odd.rbxm       # names with dots go in brackets
```

A pulled instance under a root becomes a model file. That file syncs back to Studio. Pull it again after you change it in Studio. The "Save to disk" button in the Studio toolbar does the same for the selection.

If several Studios are connected to the project (the dev place and copies), `pull` and `snapshot` refuse until you name one: `--from <session>` for a copy, `--from <placeId>` for the place. `studio-sync status` lists them.

## The whole place: snapshot and build

```bash
studio-sync snapshot     # everything in the open Studio the code files don't own -> place/ (commit it)
studio-sync build        # build/place.rbxl = code + place/ (+ place/base.rbxl's service settings)
```

`snapshot` reads Studio directly: no saved version, no API key. It replaces the snapshot dir, except `base.rbxl`. Before you commit, review with `git status` and `git diff --stat place/`. The files are binary `.rbxm`, one for each top-level instance.

If `build` says there is no `place/base.rbxl`, ask the user once for a place file (Studio: File > Save to File). Then run `studio-sync base <file>`. Without it, the built place gets default Lighting and Workspace settings, and Studio shows migration dialogs.

## Your own Studio: copies

```bash
studio-sync open --copy              # build from this checkout, open in a new Studio (background), wait until synced
studio-sync open --copy --code-only  # faster, no snapshot: code only
studio-sync close                    # done: closes this checkout's copy, no save (snapshot first if it has non-code changes)
studio-sync open                     # the project's published dev place instead
```

A copy syncs with the checkout that made it, and with nothing else. Agents in different worktrees do not need `claim`, and they do not touch the dev place of anyone.

Several Studios are then open. With the Studio MCP, `list_roblox_studios` shows the copy as `<checkout>-copy-<session>.rbxl`. Use that id. Changes to non-code content in a copy reach the repo only through `studio-sync snapshot --from <session>` or `pull ... --from <session>`. You cannot publish a copy. Run `build` and publish the file.

## Status and fixes

```bash
studio-sync status          # daemon, projects, roots, connected Studios, errors, Studio-only instances
studio-sync status --json
studio-sync log -n 80       # what synced and when (the daemon log)
studio-sync restart
```

- The Studio Output says "no project for place ...", or `status` says "Studio: not connected". The place id is not listed. Add it to `placeIds` in `studio-sync.json`, or to `studio-sync.local.json` (personal, untracked) for your own dev place.
- The message says "place ... is listed by A and B". Two checkouts (a worktree) list the same place. Run `studio-sync claim` in the checkout to sync. Studio resyncs to it within seconds. Claim back when you are done.
- The Studio Output has no `[studio-sync]` lines. The plugin is not loaded (restart Studio; `studio-sync plugin` rewrites it), or its HTTP permission was denied (Plugins > Manage Plugins > Studio Sync).
- The Sync button in the toolbar pauses the sync. While it is paused, nothing syncs in either direction. A resume makes Studio match the disk again.
- The daemon does not run. Run `studio-sync install` (once for each machine) or `studio-sync restart`.

## Set up a project

```bash
studio-sync add            # in the project root: writes studio-sync.json with default roots, registers it
```

Then set `placeIds` to a **dev place**. Never use the live place. Each person must sync their own dev place. Put it in `studio-sync.local.json` and add that file to `.gitignore`. If two people sync the same Team Create place, the syncs fight. The config reference is in `docs/config.md` of the studio-sync repo.
