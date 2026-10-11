// Studio -> disk: applies the plugin's edits (Source, new scripts and folders,
// renames, moves, deletes) to a project's files, and saves what Studio sends
// (pulled models, backups). Disk is the source of truth: an edit whose base
// is not what is on disk is a conflict, and the disk side wins.

import fs from "node:fs";
import path from "node:path";
import { BACKUP_ROOT } from "./paths.ts";
import { log } from "./log.ts";
import { keyOf, sha, type Place, type Project } from "./project.ts";

export type Op =
	| { op: "source"; path: string[]; source: string; base: string | null }
	| { op: "create"; path: string[]; className: string; source?: string; runContext?: string }
	| { op: "delete"; path: string[] }
	| { op: "move"; from: string[]; to: string[] };

export type Result = {
	ok: boolean;
	error?: string;
	sourceHash?: string;
	conflict?: { source: string; sourceHash: string; saved: string };
};

const SCRIPT_CLASSES = new Set(["Script", "LocalScript", "ModuleScript"]);
const RESERVED_SUFFIX = /\.(server|client|meta|model|project|plugin)$/i;

class OpError extends Error {}

function stamp(): string {
	return new Date().toISOString().replace(/[:.]/g, "-");
}

export function backupDir(project: Project, kind: string): string {
	return path.join(BACKUP_ROOT, project.name, `${stamp()}-${kind}`);
}

function checkName(name: string): void {
	if (
		!name ||
		name === "." ||
		name === ".." ||
		/[\\/:*?"<>|\x00-\x1f]/.test(name) ||
		/[ .]$/.test(name) ||
		name.toLowerCase() === "init" ||
		RESERVED_SUFFIX.test(name)
	) {
		throw new OpError(`"${name}" can't be a file name (no \\ / : * ? " < > |, no trailing dot or space, not "init" or *.server/.client/.meta)`);
	}
}

// The file suffix a new script gets: ModuleScript .luau, LocalScript (or a Client Script) .client.luau.
function suffixFor(className: string, runContext?: string): string {
	if (className === "ModuleScript") return ".luau";
	if (className === "LocalScript" || runContext === "Client") return ".client.luau";
	return ".server.luau";
}

// The part of a script file's name after the instance name: ".server.luau" for foo.server.luau.
function suffixOf(file: string, name: string): string {
	return path.basename(file).slice(name.length);
}

export class Writer {
	project: Project;
	// where files made since the last regenerate live (the sourcemap doesn't know them yet)
	created: Map<string, Place>;

	constructor(project: Project) {
		this.project = project;
		this.created = project.pendingPlaces;
	}

	private abs(rel: string): string {
		const full = path.resolve(this.project.dir, rel);
		if (!full.startsWith(path.resolve(this.project.dir) + path.sep)) throw new OpError(`${rel} is outside the project`);
		return full;
	}

	private place(p: string[]): Place | undefined {
		const key = keyOf(p);
		return this.created.get(key) ?? this.project.places.get(key);
	}

	private writable(p: string[]): void {
		if (!this.project.roots.some((r) => r.length <= p.length && r.every((n, i) => p[i] === n))) {
			throw new OpError(`${p.join(".")} is outside the synced roots`);
		}
		if (this.project.readOnly.some((r) => r.length <= p.length && r.every((n, i) => p[i] === n))) {
			throw new OpError(`${p.join(".")} is read-only (studio-sync.json readOnly)`);
		}
	}

	// Two siblings whose names differ only in case collide on macOS and Windows.
	private checkFree(dir: string, name: string, except?: string): void {
		const lower = name.toLowerCase();
		for (const entry of fs.readdirSync(this.abs(dir))) {
			const stem = entry.replace(/(\.(server|client))?\.(luau|lua|meta\.json|model\.json|rbxmx?|json|txt)$/i, "");
			if (stem.toLowerCase() === lower && path.join(dir, entry) !== except) {
				throw new OpError(`${path.join(dir, entry)} already exists`);
			}
		}
	}

	// The directory a child of `parent` goes in; a single-file script becomes dir/init.
	private containerDir(parent: string[]): string {
		const place = this.place(parent);
		const node = this.project.nodes.get(keyOf(parent));
		if (!place) throw new OpError(`${parent.join(".")} is not on disk`);
		if (place.dir && (!node || node.kind === "dir" || node.kind === "init" || this.created.has(keyOf(parent)))) {
			return place.dir;
		}
		if (place.file && node?.kind === "script" && !place.pinned) {
			const name = parent[parent.length - 1];
			const dir = path.join(path.dirname(place.file), name);
			fs.mkdirSync(this.abs(dir));
			const init = path.join(dir, "init" + suffixOf(place.file, name));
			fs.renameSync(this.abs(place.file), this.abs(init));
			if (place.meta) {
				fs.renameSync(this.abs(place.meta), this.abs(path.join(dir, "init.meta.json")));
			}
			log(`[${this.project.name}] ${place.file} -> ${init} (it has children now)`);
			this.created.set(keyOf(parent), { dir, file: init });
			return dir;
		}
		throw new OpError(`${parent.join(".")} can't hold synced children (it is not a folder or script on disk)`);
	}

	private sourceFile(p: string[]): string {
		const place = this.place(p);
		const node = this.project.nodes.get(keyOf(p));
		if (!place?.file || (node && node.kind !== "script" && node.kind !== "init")) {
			throw new OpError(`${p.join(".")} has no script file (generated or model content is read-only)`);
		}
		return place.file;
	}

	private moveToBackup(rel: string, into: string): void {
		const target = path.join(into, rel);
		fs.mkdirSync(path.dirname(target), { recursive: true });
		fs.cpSync(this.abs(rel), target, { recursive: true });
		fs.rmSync(this.abs(rel), { recursive: true, force: true });
	}

	apply(ops: Op[]): Result[] {
		const results: Result[] = [];
		let trash: string | null = null;
		for (const op of ops) {
			try {
				results.push(this.applyOne(op, () => (trash ??= backupDir(this.project, "deleted"))));
			} catch (e) {
				const error = e instanceof Error ? e.message : String(e);
				log(`[${this.project.name}] writeback refused: ${error}`);
				results.push({ ok: false, error });
			}
		}
		void this.project.check(true);
		return results;
	}

	private applyOne(op: Op, trash: () => string): Result {
		if (!this.project.config.writeback) throw new OpError("writeback is off for this project (studio-sync.json)");
		const name = this.project.name;
		if (op.op === "source") {
			this.writable(op.path);
			const file = this.sourceFile(op.path);
			const current = fs.readFileSync(this.abs(file), "utf8");
			const currentHash = sha(current);
			if (currentHash === sha(op.source)) return { ok: true, sourceHash: currentHash };
			if (op.base !== currentHash) {
				const dir = backupDir(this.project, "conflict");
				const saved = path.join(dir, file);
				fs.mkdirSync(path.dirname(saved), { recursive: true });
				fs.writeFileSync(saved, op.source);
				log(`[${name}] conflict on ${file}: disk changed too; kept disk, Studio's text saved to ${saved}`);
				return { ok: false, error: "conflict", conflict: { source: current, sourceHash: currentHash, saved } };
			}
			fs.writeFileSync(this.abs(file), op.source);
			log(`[${name}] <- Studio: ${file}`);
			return { ok: true, sourceHash: sha(op.source) };
		}
		if (op.op === "create") {
			this.writable(op.path);
			const leaf = op.path[op.path.length - 1];
			checkName(leaf);
			const dir = this.containerDir(op.path.slice(0, -1));
			this.checkFree(dir, leaf);
			if (op.className === "Folder") {
				const rel = path.join(dir, leaf);
				fs.mkdirSync(this.abs(rel));
				this.created.set(keyOf(op.path), { dir: rel });
				log(`[${name}] <- Studio: new folder ${rel}`);
				return { ok: true };
			}
			if (!SCRIPT_CLASSES.has(op.className)) throw new OpError(`${op.className} is saved with pull, not synced live`);
			const rel = path.join(dir, leaf + suffixFor(op.className, op.runContext));
			const source = op.source ?? "";
			fs.writeFileSync(this.abs(rel), source, { flag: "wx" });
			this.created.set(keyOf(op.path), { file: rel });
			log(`[${name}] <- Studio: new ${rel}`);
			return { ok: true, sourceHash: sha(source) };
		}
		if (op.op === "delete") {
			this.writable(op.path);
			const place = this.place(op.path);
			if (!place) return { ok: true }; // already gone
			if (place.pinned) throw new OpError(`${op.path.join(".")} is mapped in the project file; delete it there`);
			const node = this.project.nodes.get(keyOf(op.path));
			if (node && node.kind !== "script" && node.kind !== "init" && node.kind !== "dir") {
				throw new OpError(`${op.path.join(".")} comes from ${place.file}; delete that file instead`);
			}
			const rel = node?.kind === "script" || (!node && place.file && !place.dir) ? place.file! : place.dir!;
			const into = trash();
			this.moveToBackup(rel, into);
			if (place.meta && node?.kind === "script") this.moveToBackup(place.meta, into);
			log(`[${name}] <- Studio: deleted ${rel} (copy in ${into})`);
			return { ok: true };
		}
		if (op.op === "move") {
			this.writable(op.from);
			this.writable(op.to);
			const place = this.place(op.from);
			if (!place) throw new OpError(`${op.from.join(".")} is not on disk`);
			if (place.pinned) throw new OpError(`${op.from.join(".")} is mapped in the project file; rename it there`);
			const node = this.project.nodes.get(keyOf(op.from));
			const oldName = op.from[op.from.length - 1];
			const newName = op.to[op.to.length - 1];
			checkName(newName);
			const dir = this.containerDir(op.to.slice(0, -1));
			let from: string;
			let to: string;
			if (node?.kind === "script" || (!node && place.file && !place.dir)) {
				from = place.file!;
				to = path.join(dir, newName + suffixOf(from, oldName));
			} else if (node?.kind === "init" || node?.kind === "dir" || (!node && place.dir)) {
				from = place.dir!;
				to = path.join(dir, newName);
			} else {
				throw new OpError(`${op.from.join(".")} comes from ${place.file}; rename that file instead`);
			}
			this.checkFree(dir, newName, from);
			fs.renameSync(this.abs(from), this.abs(to));
			if (place.meta && node?.kind === "script") {
				fs.renameSync(this.abs(place.meta), this.abs(path.join(dir, newName + ".meta.json")));
			}
			this.created.set(keyOf(op.to), node?.kind === "script" ? { file: to } : { dir: to, file: place.file && path.join(to, path.basename(place.file)) });
			log(`[${name}] <- Studio: ${from} -> ${to}`);
			return { ok: true };
		}
		throw new OpError(`unknown op`);
	}

	// Saves a serialized instance as a model file and returns where it went.
	savePulled(instancePath: string[], file: string | undefined, data: Buffer): string {
		let rel: string;
		if (file) {
			rel = path.normalize(file);
			if (!/\.rbxmx?$/.test(rel)) rel += ".rbxm";
		} else {
			const key = keyOf(instancePath);
			const node = this.project.nodes.get(key);
			const place = this.project.places.get(key);
			if (node?.kind === "model" && place?.file) {
				rel = place.file;
			} else if (node) {
				throw new OpError(
					`${instancePath.join(".")} is a ${node.kind} on disk; scripts and folders sync by themselves, pull what is inside it`,
				);
			} else {
				this.writable(instancePath);
				const leaf = instancePath[instancePath.length - 1];
				checkName(leaf);
				const dir = this.containerDir(instancePath.slice(0, -1));
				this.checkFree(dir, leaf);
				rel = path.join(dir, leaf + ".rbxm");
			}
		}
		const full = this.abs(rel);
		fs.mkdirSync(path.dirname(full), { recursive: true });
		fs.writeFileSync(full, data);
		log(`[${this.project.name}] <- Studio: saved ${instancePath.join(".")} to ${rel} (${data.length} bytes)`);
		void this.project.check(true);
		return rel;
	}
}

// Studio's side of what a full sync is about to overwrite or remove.
export function saveBackup(project: Project, items: { path: string[]; source?: string; rbxm?: string }[]): string {
	const dir = backupDir(project, "studio");
	for (const item of items) {
		const safe = item.path.map((n) => n.replace(/[\\/:*?"<>|\x00-\x1f]/g, "_"));
		const target = path.join(dir, ...safe) + (item.source !== undefined ? ".luau" : ".rbxm");
		fs.mkdirSync(path.dirname(target), { recursive: true });
		if (item.source !== undefined) fs.writeFileSync(target, item.source);
		else if (item.rbxm) fs.writeFileSync(target, Buffer.from(item.rbxm, "base64"));
	}
	log(`[${project.name}] backed up ${items.length} Studio item(s) the sync replaced: ${dir}`);
	return dir;
}
