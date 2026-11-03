# studio-sync (work on the tool itself)

Sync between Roblox Studio and files (code one way, `.rbxm` models both ways): a Rust daemon and CLI (`src/`, one binary) and a Luau Studio plugin (`plugin/`). `README.md` is the short user guide. `docs/config.md` is the config and sync-behavior reference. `docs/protocol.md` is the HTTP and WebSocket protocol. `skill/SKILL.md` is what agents in game repos read: keep it in step with changes in behavior.

## Rules

- `cargo clippy --all-targets` (no warnings) and `cargo test` must pass. `tests/daemon.rs` is end to end on a spare port and needs `rojo` on PATH. `cargo clippy --target x86_64-pc-windows-msvc` checks the Windows code.
- The daemon never polls. File events (the `notify` crate) drive regeneration; each Studio's socket task waits on the project's `Notify`. Keep idle CPU at zero.
- Live sync needs no external tool: `src/layout.rs` maps files to instances by Rojo's rules, and `cargo test -- --include-ignored` checks it against `rojo sourcemap` on every registered project. Only `build` runs Rojo, until Roblox publishes its own place format. Don't add Lune, rbx-dom or other code that reads place files: they need updates whenever Roblox changes.
- The code must work on macOS and Windows. Use `Path::join`. Do not use shell-specific commands outside `src/install.rs` and `src/open.rs`.
- Never lose work. Copy anything that the sync overwrites or deletes to `backups/` first. Disk wins for code; Studio wins for `.rbxm` models (git can restore a file, nothing restores Studio).
- Change `version` in Cargo.toml and `PLUGIN_VERSION` in plugin/Sync.luau together. `status` flags a mismatch.

## Plugin changes

Test live in Studio without a change to the real setup:

1. Run a second daemon: `STUDIO_SYNC_HOME=<tmp> STUDIO_SYNC_PORT=34996 cargo run -- serve`.
2. Use a scratch project. Its only root is, for example, `ServerStorage.StudioSyncTest`. Its `placeIds` list the open place.
3. In the Edit context of the MCP (code there needs `HttpService.HttpEnabled`; turn it on for the test and back off after), run: `local Sync = loadstring(HttpService:GetAsync("http://localhost:34996/plugin/sync.luau"))(); local s = Sync.start({ base = "http://localhost:34996" })`.
4. Clean up the test folder after.

## Windows: not yet run there

The code is for Windows, but only macOS tested it. On the first use on Windows, check and fix these items. Keep macOS working.

1. `cargo install --path .`, `studio-sync install`: the launcher in the Startup folder starts the daemon (`studio-sync status`), the plugin goes to `%LOCALAPPDATA%\Roblox\Plugins`, and the skill goes to `~/.claude/skills/studio-sync/SKILL.md`.
2. Live sync on a dev place (`studio-sync wait`; a model edited in Studio reaches its `.rbxm`). Include a script file with CRLF line endings.
3. `snapshot`, `build`: the tool finds `rojo` (`Command` finds `.exe` shims on PATH, not `.cmd`). Test paths with spaces.
   - A project's `copy.command` that is a `.cmd` (npm, npx) needs `cmd /c` in front.
4. `open --copy`: `launch()` in `src/open.rs` runs `start "" /min "<newest %LOCALAPPDATA%\Roblox\Versions\*\RobloxStudioBeta.exe>" -task EditFile -localPlaceFile <file>`.
   - Make sure Studio opens the place (not the start page), minimized and without focus.
   - Make sure `open` (the `roblox-studio:` URI) opens the dev place.
5. `close`: `studios_with()` finds the Studio through `sysinfo` (its command line), then kills it. Make sure the file and its `.lock` are deleted, and that `<copy>_AutoRecovery_*.rbxl` files move from `%LOCALAPPDATA%\Roblox\RobloxStudio\AutoSaves` to backups.
