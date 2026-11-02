// The `studio-sync` command.

import fs from "node:fs";
import path from "node:path";
import { readRegistry, serve, writeRegistry } from "./daemon.ts";
import { buildPlugin, install, restartService, uninstall } from "./install.ts";
import { LOG_FILE, PORT, VERSION } from "./paths.ts";
import { defaultRoots } from "./project.ts";
import { build, makeBase } from "./snapshot.ts";
import { closeCopies, copies, openCopy, openPlace } from "./open.ts";

const USAGE = `studio-sync ${VERSION}: two-way sync between Roblox Studio and your files

  add [dir]              register a project (writes studio-sync.json if it does not exist)
  remove [dir]           unregister a project
  claim [dir]            make this project sync its place ids (for worktrees)
  list                   list the registered projects
  status [--json]        show the daemon, the projects, the connected Studios, and the errors
  wait [--timeout s]     wait until Studio applies every file change (exit 0).
                         Exit 2: no Studio. Exit 3: Studio reported errors. Exit 4: timeout.
  pull <Instance.Path> [file] [--from <copy session | place id>]
                         save a Studio instance into the repo as .rbxm (models, parts, maps)
  snapshot [--from <copy session | place id>]
                         save the whole open place, except the project code,
                         into the repo (snapshot.dir, default place/; base.rbxl stays)
                         With several Studios connected to the project, --from names the one.
  build [-o file] [--code-only]
                         build a place file from the project and the snapshot (default build/place.rbxl)
  base <place.rbxl>      keep the service settings of a place file (not its content)
                         as <snapshot dir>/base.rbxl. The build starts from it.
                         Run it once, and again if the Workspace or Lighting settings change.
  open [placeId]         open the dev place (or placeId) in Studio and wait for the sync
  open --copy [--code-only]
                         build a private local copy of the place from this checkout and open it.
                         The copy syncs with this checkout only (one copy for each agent or worktree).
  close [session]        close the copies of this checkout (or one copy) without a save.
                         Delete their files.
  copies [--clean]       list the copies. --clean deletes the copies that no Studio has open.
  log [-n lines]         show the daemon log
  install                install the plugin, the background service, and the Claude skill
  uninstall              remove them
  restart                restart the daemon
  plugin                 rebuild the Studio plugin only
  serve                  run the daemon in the foreground
`;

type Status = {
	version: string;
	pid: number;
	projects: {
		dir: string;
		name: string;
		placeIds: number[];
		claimed: number[];
		version: number;
		nodes: number;
		roots: string[];
		readOnly: string[];
		writeback: boolean;
		error: string | null;
		clients: { placeId: number; session: string | null; version: number; plugin: string; secondsAgo: number; errors: string[]; unsaved: string[] }[];
	}[];
};

async function api(route: string, init?: RequestInit): Promise<Response> {
	try {
		return await fetch(`http://127.0.0.1:${PORT}${route}`, init);
	} catch {
		fail("the daemon is not running: `studio-sync install` (or `studio-sync serve` in a terminal)", 1);
	}
}

function fail(message: string, code = 1): never {
	process.stderr.write(message + "\n");
	process.exit(code);
}

function target(args: string[]): string {
	const dir = args.find((a) => !a.startsWith("-")) ?? ".";
	return fs.realpathSync(path.resolve(dir));
}

// The project dir containing `from`, and its merged config (studio-sync.json + .local.json).
function projectAt(from: string): { dir: string; config: any } {
	let dir = fs.realpathSync(path.resolve(from));
	while (!fs.existsSync(path.join(dir, "studio-sync.json"))) {
		const up = path.dirname(dir);
		if (up === dir) fail(`no studio-sync.json in ${from} or above (studio-sync add)`);
		dir = up;
	}
	const read = (file: string) => (fs.existsSync(file) ? JSON.parse(fs.readFileSync(file, "utf8")) : {});
	const config = { project: "default.project.json", ...read(path.join(dir, "studio-sync.json")), ...read(path.join(dir, "studio-sync.local.json")) };
	return { dir, config };
}

function flag(args: string[], name: string): string | undefined {
	const at = args.indexOf(name);
	return at >= 0 ? args[at + 1] : undefined;
}

// The Studio a pull or snapshot goes to (`--from`), as the daemon's query, and the arguments without it.
function fromFlag(args: string[]): { from: Record<string, string>; rest: string[] } {
	const at = args.indexOf("--from");
	if (at < 0) return { from: {}, rest: args };
	const value = args[at + 1];
	if (!value || value.startsWith("-")) fail("--from needs a copy session or a place id (studio-sync status lists them)");
	return { from: { from: value }, rest: args.filter((_, i) => i !== at && i !== at + 1) };
}

// "ReplicatedStorage.shared.Thing" or "game.Workspace.Map"; names with dots go in brackets: Workspace["a.b"]
function parseInstancePath(text: string): string[] {
	const parts: string[] = [];
	const re = /\[\s*"([^"]*)"\s*\]|([^.[\]]+)/g;
	for (const m of text.matchAll(re)) parts.push(m[1] ?? m[2]);
	if (parts[0] === "game") parts.shift();
	if (!parts.length) fail(`no instance path in "${text}"`);
	return parts;
}

const commands: Record<string, (args: string[]) => Promise<void> | void> = {
	add(args) {
		const dir = target(args);
		const config = path.join(dir, "studio-sync.json");
		if (!fs.existsSync(config)) {
			const projectFile = path.join(dir, "default.project.json");
			let roots: string[][] = [];
			if (fs.existsSync(projectFile)) roots = defaultRoots(JSON.parse(fs.readFileSync(projectFile, "utf8"))).roots;
			fs.writeFileSync(
				config,
				JSON.stringify({ placeIds: [], project: "default.project.json", roots, sourcemap: true }, null, "\t") + "\n",
			);
			console.log(`wrote ${config}: put your dev place id in placeIds (or in studio-sync.local.json, untracked)`);
		}
		const registry = readRegistry();
		if (!registry.projects.includes(dir)) registry.projects.push(dir);
		writeRegistry(registry);
		console.log(`added ${dir}`);
	},
	remove(args) {
		const dir = target(args);
		const registry = readRegistry();
		registry.projects = registry.projects.filter((p) => p !== dir);
		for (const [id, owner] of Object.entries(registry.claims)) if (owner === dir) delete registry.claims[id];
		writeRegistry(registry);
		console.log(`removed ${dir}`);
	},
	claim(args) {
		const dir = target(args);
		const config = JSON.parse(fs.readFileSync(path.join(dir, "studio-sync.json"), "utf8"));
		const local = path.join(dir, "studio-sync.local.json");
		const ids: number[] = (fs.existsSync(local) ? JSON.parse(fs.readFileSync(local, "utf8")).placeIds : null) ?? config.placeIds ?? [];
		if (!ids.length) fail("this project lists no placeIds");
		const registry = readRegistry();
		if (!registry.projects.includes(dir)) registry.projects.push(dir);
		for (const id of ids) registry.claims[String(id)] = dir;
		writeRegistry(registry);
		console.log(`${dir} now syncs place(s) ${ids.join(", ")}; Studio resyncs within seconds`);
	},
	list() {
		for (const dir of readRegistry().projects) console.log(dir);
	},
	async status(args) {
		const status = (await (await api("/status")).json()) as Status;
		if (args.includes("--json")) return console.log(JSON.stringify(status, null, 2));
		console.log(`daemon ${status.version} pid ${status.pid} on port ${PORT}`);
		if (status.version !== VERSION) console.log(`  the daemon is ${status.version}, this checkout is ${VERSION}: studio-sync restart`);
		for (const p of status.projects) {
			console.log(`\n${p.name}  v${p.version}  ${p.nodes} instances  places ${p.placeIds.join(", ")}${p.claimed.length ? " (claimed)" : ""}`);
			console.log(`  ${p.dir}`);
			console.log(`  roots: ${p.roots.join(", ")}${p.readOnly.length ? `  read-only: ${p.readOnly.join(", ")}` : ""}`);
			console.log(`  writeback: ${p.writeback ? "on (two-way)" : "off (disk -> Studio)"}`);
			if (p.error) console.log(`  ERROR ${p.error}`);
			if (!p.clients.length) console.log("  Studio: not connected");
			for (const c of p.clients) {
				const lag = c.version === p.version ? "in sync" : `at v${c.version}, behind`;
				console.log(`  Studio: ${c.session ? `copy ${c.session}` : `place ${c.placeId}`}, ${lag}, plugin ${c.plugin}`);
				if (c.plugin !== VERSION) console.log(`    plugin ${c.plugin} is not ${VERSION}: run \`studio-sync plugin\` and restart Studio`);
				for (const e of c.errors) console.log(`    error: ${e}`);
				if (c.unsaved.length) {
					console.log(`    ${c.unsaved.length} instance(s) in Studio only (studio-sync pull <path> to keep): ${c.unsaved.slice(0, 5).join(", ")}${c.unsaved.length > 5 ? ", ..." : ""}`);
				}
			}
		}
	},
	async wait(args) {
		const timeout = Number(flag(args, "--timeout") ?? 30);
		const response = await api(`/wait?dir=${encodeURIComponent(process.cwd())}&timeout=${timeout}`);
		if (!response.ok) fail(await response.text());
		const result = (await response.json()) as {
			ok: boolean;
			reason?: string;
			version: number;
			error: string | null;
			project: string;
			clients: { placeId: number; version: number; errors: string[] }[];
		};
		if (result.error) fail(`${result.project}: the last sync failed: ${result.error}`, 3);
		if (!result.ok) {
			if (result.reason === "no-studio") {
				fail(`${result.project} v${result.version}: no Studio is connected (open the dev place in Edit; \`studio-sync status\`)`, 2);
			}
			fail(`${result.project}: Studio has not applied v${result.version} after ${timeout}s`, 4);
		}
		const errors = result.clients.flatMap((c) => c.errors);
		if (errors.length) fail(`${result.project} v${result.version} applied with errors:\n  ${errors.join("\n  ")}`, 3);
		console.log(`${result.project} v${result.version}: Studio is in sync`);
	},
	async pull(args) {
		const { from, rest } = fromFlag(args);
		const [instance, file] = rest;
		if (!instance) fail("usage: studio-sync pull <Instance.Path> [file] [--from <copy session | place id>]");
		const query = new URLSearchParams({
			dir: process.cwd(),
			path: JSON.stringify(parseInstancePath(instance)),
			file: file ? path.relative(target([]), path.resolve(file)) : "",
			...from,
		});
		const response = await api(`/pull/request?${query}`, { method: "POST" });
		const text = await response.text();
		let result: { ok?: boolean; file?: string; error?: string } = {};
		try {
			result = JSON.parse(text);
		} catch {
			fail(text);
		}
		if (!result.ok) fail(result.error ?? text);
		console.log(`saved ${instance} to ${result.file}`);
	},
	async snapshot(args) {
		const query = new URLSearchParams({ dir: process.cwd(), ...fromFlag(args).from });
		const response = await api(`/snapshot/request?${query}`, { method: "POST" });
		const text = await response.text();
		let result: any;
		try {
			result = JSON.parse(text);
		} catch {
			fail(text);
		}
		if (!result.ok) fail(`snapshot failed: ${result.error}`);
		console.log(`saved ${result.entries} instance(s) to ${result.dir}/:`);
		for (const line of result.top) console.log(`  ${line}`);
		if (result.duplicates?.length) console.log(`not saved, a sibling has the same name (rename them): ${result.duplicates.join(", ")}`);
		if (result.skipped?.length) console.log(`not saved: ${result.skipped.join(", ")}`);
		console.log("review with git status / git diff, then commit; `studio-sync build` makes the place file");
	},
	build(args) {
		const { dir, config } = projectAt(".");
		const output = flag(args, "-o") ?? "build/place.rbxl";
		try {
			for (const line of build(dir, config, { output, snapshot: !args.includes("--code-only") })) console.log(line);
		} catch (e) {
			fail(e instanceof Error ? e.message : String(e));
		}
	},
	base(args) {
		const { dir, config } = projectAt(".");
		if (!args[0]) fail("usage: studio-sync base <place.rbxl>  (File > Save to File in Studio, or a downloaded place)");
		try {
			for (const line of makeBase(dir, config, args[0])) console.log(line);
		} catch (e) {
			fail(e instanceof Error ? e.message : String(e));
		}
	},
	async open(args) {
		const { dir, config } = projectAt(".");
		const timeoutS = Number(flag(args, "--timeout") ?? 180);
		await api("/status"); // fails clearly when the daemon is down
		try {
			let lines: string[];
			if (args.includes("--copy")) {
				lines = await openCopy(dir, config, { codeOnly: args.includes("--code-only"), timeoutS });
			} else {
				const placeId = Number(args.find((a) => /^\d+$/.test(a)) ?? config.placeIds?.[0]);
				if (!placeId) fail("no place id: pass one, or list it in studio-sync.json placeIds");
				lines = await openPlace(dir, placeId, timeoutS);
			}
			for (const line of lines) console.log(line);
		} catch (e) {
			fail(e instanceof Error ? e.message : String(e));
		}
	},
	async close(args) {
		const { dir } = projectAt(".");
		for (const line of await closeCopies(dir, args.find((a) => !a.startsWith("-")))) console.log(line);
	},
	async copies(args) {
		for (const line of await copies(args.includes("--clean"))) console.log(line);
	},
	log(args) {
		const n = Number(flag(args, "-n") ?? 40);
		const lines = fs.existsSync(LOG_FILE) ? fs.readFileSync(LOG_FILE, "utf8").trimEnd().split("\n") : [];
		console.log(lines.slice(-n).join("\n"));
	},
	install() {
		for (const line of install()) console.log(line);
	},
	uninstall() {
		for (const line of uninstall()) console.log(line);
	},
	async restart() {
		let pid: number | null = null;
		try {
			pid = ((await (await fetch(`http://127.0.0.1:${PORT}/status`)).json()) as Status).pid;
		} catch {}
		console.log(restartService(pid));
	},
	plugin() {
		console.log(`plugin written to ${buildPlugin()}; Studio reloads it (or restart Studio)`);
	},
	serve() {
		serve();
	},
};

const [name, ...rest] = process.argv.slice(2);
if (!name || !(name in commands)) {
	process.stdout.write(USAGE);
	process.exit(name && !["help", "--help", "-h"].includes(name) ? 1 : 0);
}
await commands[name](rest);
