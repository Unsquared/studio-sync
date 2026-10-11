// One registered project: its config, the watcher, the node tree generated
// from `rojo sourcemap`, the delta log the plugin long-polls, and the Studio
// clients connected to its places.

import { spawn } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { STAGING_ROOT } from "./paths.ts";
import { log } from "./log.ts";

const PACKAGE_DIRS = new Set(["Packages", "ServerPackages", "DevPackages"]);
const SKIP_WALK = new Set([".git", "node_modules", ".DS_Store"]);
const LOG_KEEP = 300;
// A client that has not polled for this long is gone
export const CLIENT_TTL_MS = 45_000;

export type Kind =
	| "script" // a .luau file: Source syncs both ways
	| "init" // a directory with an init script: Source syncs both ways, children live in the directory
	| "dir" // a directory as a Folder (or a class from init.meta.json)
	| "generated" // .json / .txt / .toml / .csv turned into an instance: read-only
	| "model" // a .rbxm / .rbxmx file: pushed whole, saved back with `pull`
	| "json" // an instance from a .model.json: read-only
	| "project"; // an instance the project file declares

export type Node = {
	path: string[];
	className: string;
	kind: Kind;
	ro?: true;
	source?: string;
	sourceHash?: string;
	runContext?: "Server" | "Client";
	properties?: Record<string, unknown>;
	attributes?: Record<string, unknown>;
	model?: string;
	modelHash?: string;
	error?: string;
	hash: string;
};

// Where a node lives on disk (relative to the project dir); never sent to Studio.
export type Place = { file?: string; dir?: string; meta?: string; pinned?: boolean };

export type Config = {
	placeIds: number[];
	project: string;
	roots: string[][] | null;
	readOnly: string[][] | null;
	sourcemap: boolean;
	writeback: boolean;
	content: { dir: string; sources?: string[] } | null;
	snapshot: { dir?: string; ignore?: string[][] } | null;
};

export type Client = {
	id: string;
	placeId: number;
	session: string | null;
	version: number;
	lastSeen: number;
	polling: number;
	plugin: string;
	errors: string[];
	errorsVersion: number;
	unsaved: string[];
};

export type Command = { id: string; client: string; kind: "pull" | "snapshot"; path?: string[]; file?: string; taken: boolean };

type Delta = { version: number; upserts: Map<string, Node>; removes: Map<string, string[]> };

export function keyOf(p: string[]): string {
	return p.join("\u0000");
}

export function sha(text: string | Buffer): string {
	return crypto.createHash("sha1").update(text).digest("hex").slice(0, 16);
}

function readJson(file: string): any {
	return JSON.parse(fs.readFileSync(file, "utf8"));
}

function statSig(file: string): string | null {
	try {
		const st = fs.statSync(file);
		return `${st.mtimeMs}:${st.size}`;
	} catch {
		return null;
	}
}

function walkSig(dir: string, out: Map<string, string | null>): void {
	const sig = statSig(dir);
	out.set(dir, sig);
	if (sig === null) return;
	let entries: fs.Dirent[];
	try {
		entries = fs.readdirSync(dir, { withFileTypes: true });
	} catch {
		return;
	}
	for (const entry of entries) {
		if (SKIP_WALK.has(entry.name)) continue;
		const full = path.join(dir, entry.name);
		if (entry.isDirectory()) walkSig(full, out);
		else out.set(full, statSig(full));
	}
}

function sameSig(a: Map<string, string | null> | null, b: Map<string, string | null>): boolean {
	if (!a || a.size !== b.size) return false;
	for (const [k, v] of b) if (a.get(k) !== v) return false;
	return true;
}

function startsWith(p: string[], prefix: string[]): boolean {
	return prefix.length <= p.length && prefix.every((name, i) => p[i] === name);
}

// Every "$path" in a project tree, by the instance path that declares it.
function projectPaths(tree: any, here: string[], out: Map<string, string>): void {
	for (const [key, value] of Object.entries(tree ?? {})) {
		if (key === "$path" && typeof value === "string") out.set(keyOf(here), value);
		else if (!key.startsWith("$") && value && typeof value === "object") projectPaths(value, [...here, key], out);
	}
}

function isPackageDir(p: string): boolean {
	return PACKAGE_DIRS.has(path.basename(p.replace(/[\\/]+$/, "")));
}

// Default roots: every service, or service child, the project maps from a $path.
export function defaultRoots(project: any): { roots: string[][]; readOnly: string[][] } {
	const roots: string[][] = [];
	const readOnly: string[][] = [];
	const add = (p: string[], fsPath: string) => {
		roots.push(p);
		if (isPackageDir(fsPath)) readOnly.push(p);
	};
	for (const [service, body] of Object.entries<any>(project.tree ?? {})) {
		if (service.startsWith("$") || !body || typeof body !== "object") continue;
		if (typeof body.$path === "string") {
			add([service], body.$path);
			continue;
		}
		for (const [name, child] of Object.entries<any>(body)) {
			if (name.startsWith("$") || !child || typeof child !== "object") continue;
			if (typeof child.$path === "string") add([service, name], child.$path);
		}
	}
	return { roots, readOnly };
}

// JSON as a Luau table literal, the way Rojo turns a .json file into a ModuleScript.
export function luauLiteral(value: unknown, indent = ""): string {
	if (value === null || value === undefined) return "nil";
	if (typeof value === "string") return JSON.stringify(value);
	if (typeof value === "number" || typeof value === "boolean") return String(value);
	const inner = indent + "\t";
	if (Array.isArray(value)) {
		if (value.length === 0) return "{}";
		return `{\n${value.map((v) => inner + luauLiteral(v, inner) + ",").join("\n")}\n${indent}}`;
	}
	// sorted like Rojo's output, so a Rojo-built place and a synced one hold the same text
	const entries = Object.entries(value as Record<string, unknown>).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
	if (entries.length === 0) return "{}";
	const field = (k: string) => (/^[A-Za-z_][A-Za-z0-9_]*$/.test(k) ? k : `[${JSON.stringify(k)}]`);
	return `{\n${entries.map(([k, v]) => `${inner}${field(k)} = ${luauLiteral(v, inner)},`).join("\n")}\n${indent}}`;
}

function run(command: string, args: string[], cwd: string): Promise<string> {
	return new Promise((resolve, reject) => {
		const child = spawn(command, args, { cwd, shell: process.platform === "win32" });
		let out = "";
		let err = "";
		child.stdout.on("data", (d) => (out += d));
		child.stderr.on("data", (d) => (err += d));
		child.on("error", reject);
		child.on("close", (code) => {
			if (code === 0) resolve(out);
			else reject(new Error(`${command} ${args[0]}: ${err.trim() || `exit ${code}`}`));
		});
	});
}

export class Project {
	dir: string;
	name: string;
	configPath: string;
	localConfigPath: string;
	config: Config = {
		placeIds: [],
		project: "default.project.json",
		roots: null,
		readOnly: null,
		sourcemap: false,
		writeback: true,
		content: null,
		snapshot: null,
	};
	epoch = Date.now().toString(16);
	version = 0;
	logBase = 0;
	changes: Delta[] = [];
	nodes = new Map<string, Node>();
	places = new Map<string, Place>();
	pendingPlaces = new Map<string, Place>();
	roots: string[][] = [];
	readOnly: string[][] = [];
	error: string | null = null;
	lastGenerated = 0;
	clients = new Map<string, Client>();
	commands: Command[] = [];
	staging: string;
	stopped = false;

	private sig: Map<string, string | null> | null = null;
	private configSig: string | null = null;
	private waiters = new Set<() => void>();
	private checking: Promise<void> | null = null;
	private forced = false;

	constructor(dir: string) {
		this.dir = dir;
		this.name = path.basename(dir);
		this.configPath = path.join(dir, "studio-sync.json");
		this.localConfigPath = path.join(dir, "studio-sync.local.json");
		this.staging = path.join(STAGING_ROOT, sha(dir).slice(0, 10));
		void this.loop();
	}

	// -- config ---------------------------------------------------------------

	private loadConfig(): void {
		const raw = readJson(this.configPath);
		const local = fs.existsSync(this.localConfigPath) ? readJson(this.localConfigPath) : {};
		const merged = { ...raw, ...local };
		this.config = {
			placeIds: (merged.placeIds ?? []).map(Number),
			project: merged.project ?? "default.project.json",
			roots: merged.roots ?? null,
			readOnly: merged.readOnly ?? null,
			sourcemap: Boolean(merged.sourcemap),
			writeback: merged.writeback !== false,
			content: merged.content ?? null,
			snapshot: merged.snapshot ?? null,
		};
	}

	private watchSignature(): Map<string, string | null> {
		const sigs = new Map<string, string | null>();
		const projectFile = path.join(this.dir, this.config.project);
		sigs.set(projectFile, statSig(projectFile));
		let project: any;
		try {
			project = readJson(projectFile);
		} catch {
			return sigs;
		}
		const paths = new Map<string, string>();
		projectPaths(project.tree, [], paths);
		const base = path.dirname(projectFile);
		for (const p of paths.values()) {
			const full = path.normalize(path.join(base, p));
			if (isPackageDir(full) && !this.config.sourcemap && !this.coversPackages()) continue;
			walkSig(full, sigs);
		}
		return sigs;
	}

	private coversPackages(): boolean {
		return this.roots.some((r) => PACKAGE_DIRS.has(r[r.length - 1]));
	}

	// -- loop -----------------------------------------------------------------

	private async loop(): Promise<void> {
		while (!this.stopped) {
			await this.check();
			await new Promise((r) => setTimeout(r, this.error ? 2000 : 400));
		}
	}

	// Regenerates when anything watched changed. Concurrent callers share one run.
	check(force = false): Promise<void> {
		if (force) this.forced = true;
		if (!this.checking) {
			this.checking = this.checkOnce().finally(() => (this.checking = null));
		} else if (force) {
			return this.checking.then(() => this.check(true));
		}
		return this.checking;
	}

	private async checkOnce(): Promise<void> {
		try {
			const configSig = `${statSig(this.configPath)}|${statSig(this.localConfigPath)}`;
			let configChanged = false;
			if (configSig !== this.configSig) {
				this.loadConfig();
				this.configSig = configSig;
				configChanged = true;
			}
			let sig = this.watchSignature();
			if (!configChanged && !this.forced && sameSig(this.sig, sig)) return;
			if (!this.forced) {
				// let a burst of writes (a branch switch, a formatter) settle
				await new Promise((r) => setTimeout(r, 150));
				const settled = this.watchSignature();
				if (!sameSig(sig, settled)) return;
			}
			this.forced = false;
			this.sig = sig;
			await this.regenerate();
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
			log(`[${this.name}] ${this.error}`);
		}
	}

	// -- generation -----------------------------------------------------------

	private async regenerate(): Promise<void> {
		const started = Date.now();
		const config = this.config;
		const projectFile = path.join(this.dir, config.project);
		const project = readJson(projectFile);
		const [full, scriptsOnly] = await Promise.all([
			run("rojo", ["sourcemap", "--include-non-scripts", config.project], this.dir),
			config.sourcemap ? run("rojo", ["sourcemap", config.project], this.dir) : Promise.resolve(null),
		]);
		const defaults = defaultRoots(project);
		const roots = config.roots ?? defaults.roots;
		const readOnly = config.readOnly ?? defaults.readOnly.filter((r) => roots.some((root) => startsWith(r, root)));
		const mapped = new Map<string, string>();
		projectPaths(project.tree, [], mapped);
		const nodes = new Map<string, Node>();
		const places = new Map<string, Place>();
		const ctx = { roots, readOnly, mapped, nodes, places, base: path.dirname(config.project) };
		this.collect(JSON.parse(full), null, undefined, false, null, ctx);
		if (scriptsOnly !== null) this.writeSourcemap(scriptsOnly);
		this.error = null;
		this.lastGenerated = Date.now();

		const old = this.nodes;
		const upserts = new Map<string, Node>();
		for (const [k, n] of nodes) if (old.get(k)?.hash !== n.hash) upserts.set(k, n);
		const removes = new Map<string, string[]>();
		for (const [k, n] of old) if (!nodes.has(k)) removes.set(k, n.path);
		const rootsChanged = JSON.stringify([roots, readOnly]) !== JSON.stringify([this.roots, this.readOnly]);
		this.nodes = nodes;
		this.places = places;
		for (const key of this.pendingPlaces.keys()) if (places.has(key)) this.pendingPlaces.delete(key);
		this.roots = roots;
		this.readOnly = readOnly;
		if (upserts.size || removes.size || rootsChanged || this.version === 0) {
			this.version += 1;
			if (this.version === 1 || rootsChanged) {
				this.changes = [];
				this.logBase = this.version;
			} else {
				this.changes.push({ version: this.version, upserts, removes });
				if (this.changes.length > LOG_KEEP) this.logBase = this.changes.shift()!.version;
			}
			log(`[${this.name}] v${this.version}: +${upserts.size} -${removes.size} (${((Date.now() - started) / 1000).toFixed(2)}s)`);
			this.wake();
		}
	}

	private writeSourcemap(text: string): void {
		const file = path.join(this.dir, "sourcemap.json");
		try {
			if (fs.readFileSync(file, "utf8") === text) return;
		} catch {}
		fs.writeFileSync(file, text);
	}

	private collect(
		node: any,
		parent: string[] | null,
		parentDir: string | undefined,
		inside: boolean,
		model: any,
		ctx: {
			roots: string[][];
			readOnly: string[][];
			mapped: Map<string, string>;
			nodes: Map<string, Node>;
			places: Map<string, Place>;
			base: string;
		},
	): void {
		const here = parent ? [...parent, node.name] : [];
		const key = keyOf(here);
		const synced = inside || ctx.roots.some((r) => keyOf(r) === key);
		const filePaths: string[] = (node.filePaths ?? []).map((p: string) => path.normalize(p));

		let kind: Kind = model ? "json" : "project";
		const place: Place = {};
		let source: string | undefined;
		let runContext: Node["runContext"];
		let error: string | undefined;
		let meta: any = null;
		const properties: Record<string, unknown> = {};
		let modelFile: string | undefined;

		const mappedPath = ctx.mapped.get(key);
		if (mappedPath !== undefined) {
			place.pinned = true;
			const full = path.normalize(path.join(ctx.base, mappedPath));
			if (fs.existsSync(path.join(this.dir, full)) && fs.statSync(path.join(this.dir, full)).isDirectory()) {
				kind = "dir";
				place.dir = full;
			}
		} else if (!filePaths.length && parentDir !== undefined && !model) {
			const candidate = path.join(parentDir, node.name);
			if (fs.existsSync(path.join(this.dir, candidate))) {
				kind = "dir";
				place.dir = candidate;
			}
		}

		for (const file of filePaths) {
			const base = path.basename(file);
			const abs = path.join(this.dir, file);
			if (base.endsWith(".meta.json")) {
				meta = readJson(abs);
				place.meta = file;
				if (base.startsWith("init.")) {
					kind = "dir";
					place.dir = path.dirname(file);
				}
			} else if (base.endsWith(".model.json")) {
				model = readJson(abs);
				kind = "json";
				place.file = file;
			} else if (base.endsWith(".project.json")) {
				// a nested project: its instances come through the sourcemap
			} else if (/\.(luau|lua)$/.test(base)) {
				source = fs.readFileSync(abs, "utf8");
				if (base.startsWith("init.")) {
					kind = "init";
					place.dir = path.dirname(file);
				} else {
					kind = "script";
				}
				place.file = file;
				if (/\.server\.(luau|lua)$/.test(base)) runContext = "Server";
				else if (/\.client\.(luau|lua)$/.test(base)) runContext = "Client";
			} else if (/\.(rbxm|rbxmx)$/.test(base)) {
				kind = "model";
				modelFile = file;
				place.file = file;
			} else if (base.endsWith(".json")) {
				kind = "generated";
				place.file = file;
				source = `return ${luauLiteral(readJson(abs))}`; // no trailing newline, like Rojo
			} else if (base.endsWith(".txt")) {
				kind = "generated";
				place.file = file;
				properties.Value = fs.readFileSync(abs, "utf8");
			} else if (/\.(toml|csv)$/.test(base)) {
				kind = "generated";
				place.file = file;
				error = `${file}: .toml and .csv files are not synced (rojo build still includes them)`;
			}
		}

		let attributes: Record<string, unknown> | undefined;
		if (synced && here.length) {
			const ro = ctx.readOnly.some((r) => startsWith(here, r));
			if (meta?.properties) Object.assign(properties, meta.properties);
			if (model?.properties) Object.assign(properties, model.properties);
			if (meta?.attributes || model?.attributes) attributes = { ...meta?.attributes, ...model?.attributes };
			const entry: Omit<Node, "hash"> & { hash?: string } = { path: here, className: node.className, kind };
			if (ro) entry.ro = true;
			if (source !== undefined) {
				entry.source = source;
				entry.sourceHash = sha(source);
			}
			if (runContext) entry.runContext = runContext;
			if (Object.keys(properties).length) entry.properties = properties;
			if (attributes) entry.attributes = attributes;
			if (modelFile) {
				entry.model = modelFile;
				entry.modelHash = sha(fs.readFileSync(path.join(this.dir, modelFile)));
			}
			if (error) entry.error = error;
			entry.hash = sha(JSON.stringify(entry));
			ctx.nodes.set(key, entry as Node);
			ctx.places.set(key, place);
		}
		// a model file is pushed whole: its children are the file's business
		if (kind === "model") return;
		const childDir = kind === "dir" || kind === "init" ? place.dir : undefined;
		for (const child of node.children ?? []) {
			const childModel = model ? (model.children ?? []).find((c: any) => c.name === child.name) ?? {} : null;
			this.collect(child, here, childDir, synced, childModel, ctx);
		}
	}

	// -- queries --------------------------------------------------------------

	info() {
		return {
			name: this.name,
			epoch: this.epoch,
			version: this.version,
			roots: this.roots,
			writeback: this.config.writeback,
		};
	}

	tree() {
		return { ...this.info(), nodes: [...this.nodes.values()] };
	}

	wake(): void {
		for (const w of this.waiters) w();
		this.waiters.clear();
	}

	// Resolves on the next version bump or command, or after `ms`.
	private next(ms: number): Promise<void> {
		return new Promise((resolve) => {
			const timer = setTimeout(done, ms);
			const self = this;
			function done() {
				clearTimeout(timer);
				self.waiters.delete(done);
				resolve();
			}
			this.waiters.add(done);
		});
	}

	pendingCommands(client: string): Command[] {
		const mine = this.commands.filter((c) => c.client === client && !c.taken);
		for (const c of mine) c.taken = true;
		return mine;
	}

	async since(epoch: string, since: number, waitMs: number, client: string) {
		const deadline = Date.now() + waitMs;
		if (epoch !== this.epoch || since < this.logBase || since > this.version) return { ...this.info(), full: true };
		while (
			this.version === since &&
			Date.now() < deadline &&
			!this.commands.some((c) => c.client === client && !c.taken)
		) {
			await this.next(deadline - Date.now());
		}
		const commands = this.pendingCommands(client);
		if (since < this.logBase) return { ...this.info(), full: true, commands };
		const upserts = new Map<string, Node>();
		const removes = new Map<string, string[]>();
		for (const delta of this.changes) {
			if (delta.version <= since) continue;
			for (const [k, p] of delta.removes) {
				upserts.delete(k);
				removes.set(k, p);
			}
			for (const [k, n] of delta.upserts) {
				removes.delete(k);
				upserts.set(k, n);
			}
		}
		return {
			...this.info(),
			full: false,
			upserts: [...upserts.values()],
			removes: [...removes.values()],
			commands,
		};
	}

	// -- clients --------------------------------------------------------------

	touchClient(id: string, placeId: number, version: number, plugin: string, session: string | null = null): Client {
		let client = this.clients.get(id);
		if (!client) {
			client = { id, placeId, session, version, lastSeen: Date.now(), polling: 0, plugin, errors: [], errorsVersion: 0, unsaved: [] };
			this.clients.set(id, client);
			log(`[${this.name}] Studio connected (${session ? `copy ${session}` : `place ${placeId}`}, plugin ${plugin})`);
		}
		client.lastSeen = Date.now();
		client.version = version;
		client.placeId = placeId;
		client.plugin = plugin;
		this.wake();
		return client;
	}

	liveClients(): Client[] {
		const now = Date.now();
		for (const [id, c] of this.clients) {
			if (c.polling === 0 && now - c.lastSeen > CLIENT_TTL_MS) {
				this.clients.delete(id);
				log(`[${this.name}] Studio disconnected (place ${c.placeId})`);
			}
		}
		return [...this.clients.values()];
	}

	// Waits until every live Studio has applied the current version.
	async waitApplied(timeoutMs: number) {
		await this.check(true);
		const deadline = Date.now() + timeoutMs;
		for (;;) {
			const clients = this.liveClients();
			const behind = clients.filter((c) => c.version < this.version);
			if (clients.length && !behind.length) return { ok: true as const, clients };
			if (Date.now() >= deadline) return { ok: false as const, clients, reason: clients.length ? "timeout" : "no-studio" };
			await this.next(Math.min(500, deadline - Date.now()));
		}
	}

	stop(): void {
		this.stopped = true;
		this.wake();
	}
}
