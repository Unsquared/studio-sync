# studio-sync (work on the tool itself)

Sync between Roblox Studio and files (code one way, `.rbxm` models both ways): a Rust daemon and CLI (`src/`, one binary) and a Luau Studio plugin (`plugin/`). `README.md` is the short user guide. `docs/config.md` is the config and sync-behavior reference. `docs/protocol.md` is the HTTP and WebSocket protocol. `skill/SKILL.md` is what agents in game repos read: keep it in step with changes in behavior.

## Rules

- `cargo clippy --all-targets` (no warnings) and `cargo test` must pass. `tests/daemon.rs` is end to end on a spare port. `sh tests/clippy-windows.sh` checks the Windows code.
- The daemon never polls. File events (the `notify` crate) drive regeneration; each Studio's socket task waits on the project's `Notify`. Keep idle CPU at zero.
- No external tool: `src/layout.rs` maps files to instances by Rojo's rules, and `src/build.rs` builds the place from them. `cargo test -- --include-ignored` (needs `rojo`, from this repo's `rokit.toml`, as the reference only) checks both against Rojo on every registered project: run it after any change to them. The live sync never reads model or place files, because such code needs updates whenever Roblox changes: only `build` and `changes <file>` do, with rbx-dom, so a new format can break those two commands and nothing else. Keep rbx-dom current. Don't add Lune.
- The code must work on macOS and Windows. Use `Path::join`. Do not use shell-specific commands outside `src/install.rs` and `src/open.rs`.
- Never lose work. Copy anything that the sync overwrites or deletes to `backups/` first (they stay 30 days; a module generated from a data file holds no work and needs none). Disk wins for code. For `.rbxm` models the side that changed since they last agreed wins, and Studio wins when both did (git can restore a file, nothing restores Studio).
- Change `version` in Cargo.toml and `PLUGIN_VERSION` in plugin/Sync.luau together. `status` flags a mismatch.

## Plugin changes

`tests/plugin_harness.luau` runs the plugin's logic in a real Studio against a fake daemon, with no change to the real setup. Its header says how. Add a scenario for each behavior you change.

Then test it with a real daemon. The MCP may refuse HTTP to localhost (`not in the Assistant HTTP allowlist`). If it does, this needs `studio-sync install`, so ask first, and check in a copy of a scratch project. Otherwise:

1. Run a second daemon: `STUDIO_SYNC_HOME=<tmp> STUDIO_SYNC_PORT=34996 cargo run -- serve`.
2. Use a scratch project. Its only root is, for example, `ServerStorage.StudioSyncTest`. Its `placeIds` list the open place.
3. In the Edit context of the MCP (code there needs `HttpService.HttpEnabled`; turn it on for the test and back off after), run: `local Sync = loadstring(HttpService:GetAsync("http://localhost:34996/plugin/sync.luau"))(); local s = Sync.start({ base = "http://localhost:34996" })`.
4. Clean up the test folder after.
