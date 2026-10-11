# studio-sync (work on the tool itself)

Two-way sync between Roblox Studio and files: a Node daemon (`src/`, TypeScript that Node 23.6+ runs directly, stdlib only, no build) and a Luau Studio plugin (`plugin/`). `README.md` is the short user guide. `docs/config.md` is the config and sync-behavior reference. `docs/protocol.md` is the HTTP API. `skill/SKILL.md` is what agents in game repos read: keep it in step with changes in behavior.

## Rules

- `npm run check` (tsc) and `npm test` must pass. `npm test` is end to end on a spare port and needs `rojo` on PATH.
- Use erasable TypeScript only (no enums, namespaces, or parameter properties). Relative imports end in `.ts`.
- The code must work on macOS and Windows. Use `path.join`. Do not use shell-specific commands outside `src/install.ts`.
- Never lose work. Copy anything that the sync overwrites or deletes to `backups/` first. Disk wins conflicts.
- Change `version` in package.json and `PLUGIN_VERSION` in plugin/Sync.luau together. `status` flags a mismatch.

## Plugin changes

Test live in Studio without a change to the real setup:

1. Run a second daemon: `STUDIO_SYNC_HOME=<tmp> STUDIO_SYNC_PORT=34996 node src/cli.ts serve`.
2. Use a scratch project. Its only root is, for example, `ServerStorage.StudioSyncTest`. Its `placeIds` list the open place.
3. In the Edit context of the MCP, run: `local Sync = loadstring(HttpService:GetAsync("http://localhost:34996/plugin/sync.luau"))(); local s = Sync.start({ base = "http://localhost:34996" })`.
4. Clean up the test folder after.

## Windows: not yet run there

The code is for Windows, but only macOS tested it. On the first use on Windows, check and fix these items. Keep macOS working.

1. `npm link`, `studio-sync install`: the launcher in the Startup folder starts the daemon (`studio-sync status`), the plugin goes to `%LOCALAPPDATA%\Roblox\Plugins`, and the skill junction goes to `~/.claude/skills`.
2. Live sync in both directions on a dev place (`studio-sync wait`, an edit in Studio reaches the file). Include a file with CRLF line endings.
3. `snapshot`, `base`, `build`: the tool finds `rojo` and `lune` (`spawnSync` uses `shell: true` on Windows). Test paths with spaces.
4. `open --copy`: `launch()` in `src/open.ts` runs `start "" /min <file>`. This needs the `.rbxl` file association.
   - If Studio opens on its start page, start `RobloxStudioBeta.exe -task EditFile -localPlaceFile <file>`.
   - Find the exe through the `roblox-studio` protocol handler in the registry.
   - Do the same check for `open` (the `roblox-studio:` URI).
