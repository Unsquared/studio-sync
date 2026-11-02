// Snapshots and builds: the open Studio's whole place, minus what the project's
// files own, saved into the repo (`snapshot.dir`, default "place/"), and a place
// file built from the project plus that snapshot, so what ships is one commit.
//
// place/snapshot.json   what was saved: every instance's path and file, each
//                       service's saved properties and attributes (Rojo's
//                       explicit value form), what couldn't be saved
// place/<Service>/...   one .rbxm per top-level instance

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { ROOT, STAGING_ROOT } from "./paths.ts";
import { log } from "./log.ts";
import { keyOf, sha, type Project } from "./project.ts";
import { backupDir } from "./writeback.ts";

export type Manifest = {
	createdAt: string;
	placeId: number;
	entries: { path: string[]; className: string; file: string }[];
	services: Record<string, { className: string; properties?: Record<string, unknown>; attributes?: Record<string, unknown> }>;
	skipped: string[];
	duplicates: string[];
};

type PlanNode = { owned: boolean; children: Record<string, PlanNode> };

function readJson(file: string): any {
	return JSON.parse(fs.readFileSync(file, "utf8"));
}

export function snapshotDir(project: Project): string {
	return project.config.snapshot?.dir ?? "place";
}

// The project file's tree as the plugin needs it: which instances the files own.
export function plan(project: Project) {
	const tree = readJson(path.join(project.dir, project.config.project)).tree ?? {};
	const toNode = (body: any): PlanNode => {
		const children: Record<string, PlanNode> = {};
		for (const [name, child] of Object.entries<any>(body ?? {})) {
			if (!name.startsWith("$") && child && typeof child === "object") children[name] = toNode(child);
		}
		return { owned: typeof body?.$path === "string", children };
	};
	return { tree: toNode(tree).children, ignore: project.config.snapshot?.ignore ?? [] };
}

function safeSegment(name: string): string {
	const cleaned = name.replace(/[\\/:*?"<>|\x00-\x1f]/g, "_").replace(/[ .]+$/, "_");
	return cleaned || "_";
}

type Upload = { dir: string; files: Map<string, string>; parts: Map<string, string[]> };
const uploads = new Map<string, Upload>();

function uploadFor(project: Project, token: string): Upload {
	let upload = uploads.get(token);
	if (!upload) {
		const dir = path.join(STAGING_ROOT, `snapshot-${sha(project.dir + token).slice(0, 10)}`);
		fs.rmSync(dir, { recursive: true, force: true });
		fs.mkdirSync(dir, { recursive: true });
		upload = { dir, files: new Map(), parts: new Map() };
		uploads.set(token, upload);
	}
	return upload;
}

// One part of one instance's .rbxm, base64.
export function receiveFile(project: Project, token: string, instancePath: string[], body: string, last: boolean): void {
	const upload = uploadFor(project, token);
	const key = keyOf(instancePath);
	const parts = upload.parts.get(key) ?? [];
	parts.push(body);
	upload.parts.set(key, parts);
	if (!last) return;
	upload.parts.delete(key);
	// file systems that ignore case would merge "Map" and "map"
	const taken = new Set([...upload.files.values()].map((f) => f.toLowerCase()));
	const base = path.join(...instancePath.map(safeSegment));
	let rel = base + ".rbxm";
	for (let n = 2; taken.has(rel.toLowerCase()); n++) rel = `${base}~${n}.rbxm`;
	const full = path.join(upload.dir, rel);
	fs.mkdirSync(path.dirname(full), { recursive: true });
	fs.writeFileSync(full, Buffer.from(parts.join(""), "base64"));
	upload.files.set(key, rel);
}

// Replaces the snapshot dir with the upload (the old one goes to backups).
export function commit(project: Project, token: string, report: Omit<Manifest, "createdAt" | "entries"> & { entries: { path: string[]; className: string }[] }): string {
	const upload = uploads.get(token) ?? uploadFor(project, token);
	uploads.delete(token);
	const manifest: Manifest = {
		createdAt: new Date().toISOString(),
		placeId: report.placeId,
		entries: report.entries.map((e) => {
			const file = upload.files.get(keyOf(e.path));
			if (!file) throw new Error(`${e.path.join(".")} was never uploaded`);
			return { ...e, file: file.split(path.sep).join("/") };
		}),
		services: report.services,
		skipped: report.skipped,
		duplicates: report.duplicates,
	};
	fs.writeFileSync(path.join(upload.dir, "snapshot.json"), JSON.stringify(manifest, null, "\t") + "\n");
	const target = path.join(project.dir, snapshotDir(project));
	// the base place is `base`'s, not the snapshot's: it stays (instances live in service folders, so no name clashes)
	const base = path.join(target, "base.rbxl");
	if (fs.existsSync(base)) fs.copyFileSync(base, path.join(upload.dir, "base.rbxl"));
	if (fs.existsSync(target)) {
		const backup = backupDir(project, "snapshot");
		fs.mkdirSync(path.dirname(backup), { recursive: true });
		fs.cpSync(target, backup, { recursive: true });
		fs.rmSync(target, { recursive: true, force: true });
	}
	fs.mkdirSync(path.dirname(target), { recursive: true });
	fs.cpSync(upload.dir, target, { recursive: true });
	fs.rmSync(upload.dir, { recursive: true, force: true });
	log(`[${project.name}] snapshot: ${manifest.entries.length} instance(s) -> ${snapshotDir(project)}/`);
	return snapshotDir(project);
}

// -- build --------------------------------------------------------------------

export type BuildOptions = { output: string; snapshot: boolean; attributes?: Record<string, unknown> };

// The project tree with the snapshot grafted in. The project's own entries win.
export function buildProject(dir: string, config: { project: string; snapshot?: { dir?: string } | null }, options: BuildOptions) {
	const projectFile = path.join(dir, config.project);
	const project = readJson(projectFile);
	const base = path.dirname(projectFile);
	const tree = (project.tree ??= { $className: "DataModel" });
	const notes: string[] = [];
	const snapDir = path.join(dir, config.snapshot?.dir ?? "place");
	const manifestFile = path.join(snapDir, "snapshot.json");
	if (options.snapshot && fs.existsSync(manifestFile)) {
		const manifest: Manifest = readJson(manifestFile);
		const relDir = path.relative(base, snapDir);
		for (const [service, settings] of Object.entries(manifest.services)) {
			const node = (tree[service] ??= { $className: settings.className });
			if (settings.properties) node.$properties = { ...settings.properties, ...node.$properties };
			if (settings.attributes) node.$attributes = { ...settings.attributes, ...node.$attributes };
		}
		for (const entry of manifest.entries) {
			let node = tree;
			for (const name of entry.path.slice(0, -1)) node = node[name] ??= {};
			const leaf = entry.path[entry.path.length - 1];
			if (node[leaf]) {
				notes.push(`${entry.path.join(".")}: the project file declares it; the snapshot's copy is left out`);
				continue;
			}
			node[leaf] = { $path: path.join(relDir, entry.file).split(path.sep).join("/") };
		}
		notes.push(`snapshot from ${manifest.createdAt}: ${manifest.entries.length} instance(s)`);
	} else if (options.snapshot) {
		notes.push(`no snapshot in ${path.relative(dir, snapDir)}/ (studio-sync snapshot): code only`);
	}
	if (options.attributes) {
		const workspace = (tree.Workspace ??= { $className: "Workspace" });
		workspace.$attributes = { ...workspace.$attributes, ...options.attributes };
	}
	return { project, base, notes };
}

function run(command: string, args: string[], cwd: string): string {
	const result = spawnSync(command, args, { cwd, encoding: "utf8", shell: process.platform === "win32" });
	if (result.error) throw new Error(`${command}: ${result.error.message} (is it installed? rokit install)`);
	if (result.status !== 0) throw new Error(`${command} ${args[0]}: ${(result.stderr || result.stdout).trim()}`);
	return result.stdout;
}

// Code + snapshot through Rojo; then, when there is a base place, onto it (Lune),
// for the service settings a plugin can't read.
export function build(dir: string, config: { project: string; snapshot?: { dir?: string } | null }, options: BuildOptions): string[] {
	const { project, base, notes } = buildProject(dir, config, options);
	const output = path.resolve(dir, options.output);
	fs.mkdirSync(path.dirname(output), { recursive: true });
	const snapDir = path.join(dir, config.snapshot?.dir ?? "place");
	const basePlace = path.join(snapDir, "base.rbxl");
	const useBase = options.snapshot && fs.existsSync(basePlace);
	// written next to the project file so every $path resolves the same way
	const temp = path.join(base, `.studio-sync-build-${process.pid}.project.json`);
	const rojoOut = useBase ? path.join(STAGING_ROOT, `build-${process.pid}.rbxl`) : output;
	fs.mkdirSync(STAGING_ROOT, { recursive: true });
	fs.writeFileSync(temp, JSON.stringify(project, null, "\t"));
	try {
		run("rojo", ["build", temp, "-o", rojoOut], dir);
	} finally {
		fs.rmSync(temp, { force: true });
	}
	if (useBase) {
		const manifest: Manifest = readJson(path.join(snapDir, "snapshot.json"));
		const readable: Record<string, string[]> = {};
		for (const [service, settings] of Object.entries(manifest.services)) readable[service] = Object.keys(settings.properties ?? {});
		const readableFile = path.join(STAGING_ROOT, `readable-${process.pid}.json`);
		fs.writeFileSync(readableFile, JSON.stringify(readable));
		try {
			run("lune", ["run", path.join(ROOT, "lune", "merge.luau"), basePlace, rojoOut, output, readableFile], ROOT); // lune is pinned in our rokit.toml
		} finally {
			fs.rmSync(rojoOut, { force: true });
			fs.rmSync(readableFile, { force: true });
		}
		notes.push(`service settings from ${path.relative(dir, basePlace)}`);
	} else if (options.snapshot) {
		notes.push(
			`no ${path.relative(dir, basePlace)}: settings plugins can't read (ExplicitAutoJoints, SignalBehavior, Lighting.Technology, ...) are Roblox defaults. \`studio-sync base <place.rbxl>\` once fixes that`,
		);
	}
	return [...notes, `built ${options.output}`];
}

// Saves a place's service settings, without its content, as the base place.
export function makeBase(dir: string, config: { snapshot?: { dir?: string } | null }, source: string): string[] {
	const target = path.join(dir, config.snapshot?.dir ?? "place", "base.rbxl");
	fs.mkdirSync(path.dirname(target), { recursive: true });
	const out = run("lune", ["run", path.join(ROOT, "lune", "base.luau"), path.resolve(source), target], ROOT); // lune is pinned in our rokit.toml
	return [out.trim(), `wrote ${path.relative(dir, target)}`];
}
