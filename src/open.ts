// Opening Studio for an agent: the project's dev place (`open`), or a private
// local copy built from this checkout (`open --copy`) that syncs back to it.
// A copy is a .rbxl with a session id in a Workspace attribute; the plugin
// sends it, and the daemon maps it to the checkout that made the copy.

import { spawn, spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { BACKUP_ROOT, PORT, ROOT, STATE_DIR } from "./paths.ts";
import { readRegistry, writeRegistry } from "./daemon.ts";
import { build, run } from "./snapshot.ts";

export const COPIES_DIR = path.join(STATE_DIR, "copies");

type StatusClient = { placeId: number; session: string | null; version: number; errors: string[] };
type StatusProject = { dir: string; name: string; version: number; clients: StatusClient[] };

async function status(): Promise<StatusProject[]> {
	const response = await fetch(`http://127.0.0.1:${PORT}/status`);
	return ((await response.json()) as { projects: StatusProject[] }).projects;
}

// In the background: Studio doesn't take focus from whatever the user is doing.
function launch(target: string, isFile: boolean): void {
	let child;
	if (process.platform === "darwin") {
		// -n: a new Studio even when one is running; a file given to `open -a` alone lands on the start page
		const app = ["/Applications", path.join(os.homedir(), "Applications")].map((d) => path.join(d, "RobloxStudio.app")).find((a) => fs.existsSync(a));
		if (isFile && !app) throw new Error("RobloxStudio.app is not in /Applications or ~/Applications");
		// by name, `open -a RobloxStudio` doesn't start a second Studio
		const args = isFile ? ["-g", "-n", "-a", app!, "--args", "-task", "EditFile", "-localPlaceFile", target] : ["-g", target];
		child = spawn("open", args, { detached: true, stdio: "ignore" });
	} else if (process.platform === "win32") {
		// `start /min` opens minimized and without focus. A file goes straight to Studio's exe:
		// through the file association it can land on the start page, as on macOS.
		const exe = isFile ? windowsStudio() : null;
		if (isFile && !exe) throw new Error("RobloxStudioBeta.exe not found under %LOCALAPPDATA%\\Roblox\\Versions (open Studio once to install it)");
		const command = exe ? `"${exe}" -task EditFile -localPlaceFile "${target}"` : `"${target}"`;
		child = spawn("cmd", ["/c", `start "" /min ${command}`], { detached: true, stdio: "ignore", windowsVerbatimArguments: true });
	} else {
		throw new Error("opening Studio is supported on macOS and Windows");
	}
	child.unref();
}

// The newest installed Studio: %LOCALAPPDATA%\Roblox\Versions\version-*\RobloxStudioBeta.exe.
function windowsStudio(): string | null {
	const versions = path.join(process.env.LOCALAPPDATA ?? path.join(os.homedir(), "AppData", "Local"), "Roblox", "Versions");
	let best: { exe: string; mtime: number } | null = null;
	try {
		for (const version of fs.readdirSync(versions)) {
			const exe = path.join(versions, version, "RobloxStudioBeta.exe");
			if (!fs.existsSync(exe)) continue;
			const mtime = fs.statSync(exe).mtimeMs;
			if (!best || mtime > best.mtime) best = { exe, mtime };
		}
	} catch {}
	return best?.exe ?? null;
}

// Waits until a Studio matching `match` is connected to the project at `dir` and has applied its version.
async function waitConnected(dir: string, match: (c: StatusClient) => boolean, timeoutS: number): Promise<string> {
	const deadline = Date.now() + timeoutS * 1000;
	for (;;) {
		const project = (await status()).find((p) => p.dir === dir);
		const client = project?.clients.find(match);
		if (project && client && client.version === project.version) {
			return client.errors.length ? `connected with errors:\n  ${client.errors.join("\n  ")}` : `connected and in sync (v${project.version})`;
		}
		if (Date.now() > deadline) {
			throw new Error(
				client
					? `Studio connected but has not applied v${project?.version} yet`
					: `Studio did not connect within ${timeoutS}s (is the plugin installed, and HTTP allowed? studio-sync status)`,
			);
		}
		await new Promise((r) => setTimeout(r, 1000));
	}
}

async function universeOf(placeId: number): Promise<number> {
	const response = await fetch(`https://apis.roblox.com/universes/v1/places/${placeId}/universe`);
	if (!response.ok) throw new Error(`could not look up the universe of place ${placeId}: ${response.status}`);
	const { universeId } = (await response.json()) as { universeId: number | null };
	if (!universeId) throw new Error(`place ${placeId} has no universe (is it published?)`);
	return universeId;
}

export async function openPlace(dir: string, placeId: number, timeoutS: number): Promise<string[]> {
	const project = (await status()).find((p) => p.dir === dir);
	if (project?.clients.some((c) => c.placeId === placeId && !c.session)) {
		return [`place ${placeId} is already open and connected`];
	}
	const universeId = await universeOf(placeId);
	launch(`roblox-studio:1+launchmode:edit+task:EditPlace+placeId:${placeId}+universeId:${universeId}`, false);
	const state = await waitConnected(dir, (c) => c.placeId === placeId && !c.session, timeoutS);
	return [`opened place ${placeId}: ${state}`];
}

export type CopyConfig = { command: string[]; output: string };

// A project that builds its own full place (`copy`): run its command, then stamp the session.
function buildWithCommand(dir: string, copy: CopyConfig, file: string, session: string): string[] {
	if (!Array.isArray(copy.command) || !copy.command.length || typeof copy.output !== "string") {
		throw new Error('studio-sync.json "copy" needs "command" (a list: the program, then its arguments) and "output" (the place file it writes)');
	}
	const [command, ...args] = copy.command;
	run(command, args, dir);
	const output = path.resolve(dir, copy.output);
	if (!fs.existsSync(output)) throw new Error(`${copy.command.join(" ")} did not write ${copy.output}`);
	run("lune", ["run", path.join(ROOT, "lune", "session.luau"), output, file, session], ROOT); // lune is pinned in our rokit.toml
	return [`built ${copy.output} with ${copy.command.join(" ")}`];
}

export async function openCopy(
	dir: string,
	config: { project: string; snapshot?: { dir?: string } | null; copy?: CopyConfig | null },
	options: { codeOnly: boolean; timeoutS: number },
): Promise<string[]> {
	const session = crypto.randomBytes(4).toString("hex");
	fs.mkdirSync(COPIES_DIR, { recursive: true });
	const file = path.join(COPIES_DIR, `${path.basename(dir)}-copy-${session}.rbxl`);
	const lines =
		config.copy && !options.codeOnly
			? buildWithCommand(dir, config.copy, file, session)
			: build(dir, config, {
					output: file,
					snapshot: !options.codeOnly,
					attributes: { StudioSyncSession: { String: session } },
				});
	const registry = readRegistry();
	if (!registry.projects.includes(dir)) registry.projects.push(dir);
	registry.sessions[session] = { dir, file, createdAt: new Date().toISOString() };
	writeRegistry(registry);
	// the daemon reads the registry every second
	await new Promise((r) => setTimeout(r, 1500));
	launch(file, true);
	const state = await waitConnected(dir, (c) => c.session === session, options.timeoutS);
	return [...lines, `opened copy ${session} (${path.basename(file)}): ${state}`, "Studio edits in this copy sync to this checkout only"];
}

// Copies whose Studio is gone are deleted with `clean`.
export async function copies(clean: boolean): Promise<string[]> {
	const registry = readRegistry();
	let live = new Set<string>();
	try {
		live = new Set((await status()).flatMap((p) => p.clients.map((c) => c.session).filter((s): s is string => !!s)));
	} catch {}
	const lines: string[] = [];
	for (const [id, session] of Object.entries(registry.sessions)) {
		const open = live.has(id);
		if (clean && !open) {
			fs.rmSync(session.file, { force: true });
			delete registry.sessions[id];
			lines.push(`removed ${id} (${path.basename(session.file)})`);
		} else {
			lines.push(`${id}  ${open ? "open" : "closed"}  ${session.dir}  ${session.createdAt}`);
		}
	}
	if (clean) writeRegistry(registry);
	return lines.length ? lines : ["no copies"];
}

// Studio processes with `file` on their command line.
function studiosWith(file: string): number[] {
	const listing =
		process.platform === "win32"
			? spawnSync(
					"powershell",
					["-NoProfile", "-Command", "Get-CimInstance Win32_Process -Filter \"Name like 'RobloxStudio%'\" | ForEach-Object { \"$($_.ProcessId) $($_.CommandLine)\" }"],
					{ encoding: "utf8" },
				).stdout
			: spawnSync("ps", ["-axo", "pid=,command="], { encoding: "utf8" }).stdout;
	return (listing ?? "")
		.split(/\r?\n/)
		.filter((line) => /RobloxStudio/i.test(line) && line.includes(file))
		.map((line) => Number(line.trim().split(/\s+/)[0]));
}

// Closes copies (no save: code is already on disk; snapshot first to keep anything else),
// deletes their files and sessions. `which`: a session id, else every copy of `dir`.
export async function closeCopies(dir: string, which: string | undefined): Promise<string[]> {
	const registry = readRegistry();
	const ids = Object.keys(registry.sessions).filter((id) => (which ? id === which : registry.sessions[id].dir === dir));
	if (!ids.length) return [which ? `no copy ${which}` : "no copies of this checkout"];
	const lines: string[] = [];
	for (const id of ids) {
		const { file } = registry.sessions[id];
		const pids = studiosWith(file);
		// No graceful close: Studio ignores SIGTERM on macOS, and a built copy always gets a
		// "Save changes?" prompt. A copy is disposable; its code is on disk already.
		for (const pid of pids) {
			if (process.platform === "win32") spawnSync("taskkill", ["/F", "/PID", String(pid)], { stdio: "ignore" });
			else {
				try {
					process.kill(pid, "SIGKILL");
				} catch {}
			}
		}
		await new Promise((r) => setTimeout(r, 300));
		const recovered = await moveRecoveryFiles(file, path.basename(dir));
		if (recovered) lines.push(recovered);
		fs.rmSync(file, { force: true });
		fs.rmSync(file + ".lock", { force: true });
		delete registry.sessions[id];
		lines.push(`closed ${id}${pids.length ? "" : " (no Studio had it open)"}, deleted ${path.basename(file)}`);
	}
	writeRegistry(registry);
	return lines;
}

// Where Studio writes auto-recovery files; it has moved them between versions.
function autoSaveDirs(): string[] {
	const home = os.homedir();
	if (process.platform === "win32") {
		return [
			path.join(process.env.LOCALAPPDATA ?? path.join(home, "AppData", "Local"), "Roblox", "RobloxStudio", "AutoSaves"),
			path.join(home, "Documents", "ROBLOX", "AutoSaves"),
		];
	}
	return [path.join(home, "Library", "Application Support", "Roblox", "RobloxStudio", "AutoSaves"), path.join(home, "Documents", "ROBLOX", "AutoSaves")];
}

// Studio deletes `<place>_AutoRecovery_<n>.rbxl` only when it closes by itself. After a kill,
// the next Studio would offer to recover the copy, so they go to backups (they are normal
// place files, in case a copy had unsaved work).
async function moveRecoveryFiles(file: string, projectName: string): Promise<string | null> {
	const prefix = `${path.basename(file, ".rbxl")}_AutoRecovery_`.toLowerCase();
	const found = autoSaveDirs().flatMap((d) => {
		try {
			return fs.readdirSync(d).filter((f) => f.toLowerCase().startsWith(prefix)).map((f) => path.join(d, f));
		} catch {
			return [];
		}
	});
	if (!found.length) return null;
	const target = path.join(BACKUP_ROOT, projectName, `${new Date().toISOString().replace(/[:.]/g, "-")}-recovery`);
	fs.mkdirSync(target, { recursive: true });
	for (const from of found) {
		// a killed process can hold the file a moment longer on Windows
		for (let tries = 0; ; tries++) {
			try {
				fs.cpSync(from, path.join(target, path.basename(from)));
				fs.rmSync(from);
				break;
			} catch (e) {
				if (tries >= 20) throw e;
				await new Promise((r) => setTimeout(r, 250));
			}
		}
	}
	return `moved ${found.length} auto-recovery file(s) to ${target}`;
}
