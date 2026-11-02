// The studio-sync daemon: one HTTP server on localhost for every registered
// project. Studio finds its project by place id; the CLI finds it by directory.
// Protocol: docs/protocol.md.

import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { PLUGIN_SOURCE_DIR, PORT, PROJECTS_FILE, STATE_DIR, VERSION } from "./paths.ts";
import { log, logToFile } from "./log.ts";
import { Project, type Client, type Command } from "./project.ts";
import { commit, plan, receiveFile } from "./snapshot.ts";
import { Writer, saveBackup, type Op } from "./writeback.ts";

const LONG_POLL_MS = 20_000;
const MAX_BODY = 64 * 1024 * 1024;
const CONTENT_FILE = /^\/content\/file\/([A-Za-z]+)\/([A-Za-z0-9_]+)\.rbxm$/;

export type Session = { dir: string; file: string; createdAt: string };
type Registry = { projects: string[]; claims: Record<string, string>; sessions: Record<string, Session> };

export function readRegistry(): Registry {
	try {
		const raw = JSON.parse(fs.readFileSync(PROJECTS_FILE, "utf8"));
		return { projects: raw.projects ?? [], claims: raw.claims ?? {}, sessions: raw.sessions ?? {} };
	} catch {
		return { projects: [], claims: {}, sessions: {} };
	}
}

export function writeRegistry(registry: Registry): void {
	fs.mkdirSync(STATE_DIR, { recursive: true });
	fs.writeFileSync(PROJECTS_FILE, JSON.stringify(registry, null, 2) + "\n");
}

const projects = new Map<string, Project>();
let claims: Record<string, string> = {};
let sessions: Record<string, Session> = {};
let registrySig = "";

function refreshRegistry(): void {
	let sig = "";
	try {
		const st = fs.statSync(PROJECTS_FILE);
		sig = `${st.mtimeMs}:${st.size}`;
	} catch {}
	if (sig === registrySig) return;
	registrySig = sig;
	const registry = readRegistry();
	claims = registry.claims;
	sessions = registry.sessions;
	for (const [dir, project] of projects) {
		if (!registry.projects.includes(dir)) {
			project.stop();
			projects.delete(dir);
			log(`removed ${dir}`);
		}
	}
	for (const dir of registry.projects) {
		if (!projects.has(dir) && fs.existsSync(path.join(dir, "studio-sync.json"))) {
			projects.set(dir, new Project(dir));
			log(`added ${dir}`);
		}
	}
}

type Lookup = { project: Project } | { status: number; message: string };

function byPlace(raw: string | null, session?: string | null): Lookup {
	if (session) {
		const owner = sessions[session] && projects.get(sessions[session].dir);
		if (owner) return { project: owner };
		return { status: 404, message: `no project for copy ${session} (studio-sync copies)` };
	}
	const placeId = Number(raw);
	if (!placeId) return { status: 404, message: "no placeId (publish the place: a local file has place id 0)" };
	const claimed = claims[String(placeId)];
	if (claimed && projects.get(claimed)?.config.placeIds.includes(placeId)) return { project: projects.get(claimed)! };
	const owners = [...projects.values()].filter((p) => p.config.placeIds.includes(placeId));
	if (owners.length === 1) return { project: owners[0] };
	if (owners.length > 1) {
		return {
			status: 409,
			message: `place ${placeId} is listed by ${owners.map((p) => p.dir).join(" and ")}: run \`studio-sync claim\` in the one to sync`,
		};
	}
	return { status: 404, message: `no project for place ${placeId}` };
}

function byDir(raw: string | null): Project | undefined {
	if (!raw) return undefined;
	let dir = path.resolve(raw);
	for (;;) {
		const found = projects.get(dir);
		if (found) return found;
		const up = path.dirname(dir);
		if (up === dir) return undefined;
		dir = up;
	}
}

function status() {
	return {
		version: VERSION,
		port: PORT,
		pid: process.pid,
		projects: [...projects.values()].map((p) => ({
			dir: p.dir,
			name: p.name,
			placeIds: p.config.placeIds,
			claimed: p.config.placeIds.filter((id) => claims[String(id)] === p.dir),
			version: p.version,
			nodes: p.nodes.size,
			roots: p.roots.map((r) => r.join(".")),
			readOnly: p.readOnly.map((r) => r.join(".")),
			writeback: p.config.writeback,
			sourcemap: p.config.sourcemap,
			error: p.error,
			clients: p.liveClients().map((c) => ({
				placeId: c.placeId,
				session: c.session,
				version: c.version,
				plugin: c.plugin,
				secondsAgo: Math.round((Date.now() - c.lastSeen) / 1000),
				errors: c.errors,
				unsaved: c.unsaved,
			})),
		})),
	};
}

// -- commands: the CLI asks a project's Studio to do something (pull, snapshot) ----

type CommandResult = { ok: boolean; file?: string; dir?: string; error?: string; [key: string]: unknown };
const waiters = new Map<string, (value: CommandResult) => void>();
const pullParts = new Map<string, string[]>();

function settle(id: string, value: CommandResult): void {
	waiters.get(id)?.(value);
}

function describeClient(c: Client): string {
	return c.session ? `copy ${c.session}` : `place ${c.placeId}`;
}

// The Studio a command goes to: the one named by `from` (a copy's session or a place id), else the only
// one connected. Several Studios and no `from` is refused: which one polled last is chance.
function commandTarget(project: Project, from: string | null): Client {
	const live = project.liveClients();
	if (!live.length) throw new HttpError(409, "no Studio is connected to this project (open the place in Edit; studio-sync status)");
	const connected = live.map(describeClient).join(", ");
	if (from) {
		const matches = live.filter((c) => c.session === from || (!c.session && String(c.placeId) === from));
		if (matches.length === 1) return matches[0];
		if (!matches.length) throw new HttpError(409, `no connected Studio is ${from} (connected: ${connected})`);
		throw new HttpError(409, `${matches.length} Studios are ${from}: close all but one`);
	}
	if (live.length === 1) return live[0];
	throw new HttpError(409, `${live.length} Studios are connected to this project (${connected}): name one with --from <copy session or place id>`);
}

// Queues a command for one Studio (`commandTarget`) and waits for its result.
function runCommand(
	project: Project,
	command: Omit<Command, "id" | "client" | "taken">,
	timeoutMs: number,
	from: string | null,
): Promise<CommandResult> {
	const target = commandTarget(project, from);
	const id = crypto.randomUUID();
	return new Promise((resolve) => {
		const timer = setTimeout(() => {
			project.commands = project.commands.filter((c) => c.id !== id);
			settle(id, { ok: false, error: `Studio did not answer in ${timeoutMs / 1000} s` });
		}, timeoutMs);
		waiters.set(id, (value) => {
			clearTimeout(timer);
			waiters.delete(id);
			resolve(value);
		});
		project.commands.push({ ...command, id, client: target.id, taken: false });
		project.wake();
	});
}

// -- HTTP -----------------------------------------------------------------------

class HttpError extends Error {
	status: number;
	constructor(status: number, message: string) {
		super(message);
		this.status = status;
	}
}

function readBody(req: http.IncomingMessage): Promise<Buffer> {
	return new Promise((resolve, reject) => {
		const chunks: Buffer[] = [];
		let size = 0;
		req.on("data", (chunk: Buffer) => {
			size += chunk.length;
			if (size > MAX_BODY) reject(new HttpError(413, "body too large"));
			else chunks.push(chunk);
		});
		req.on("end", () => resolve(Buffer.concat(chunks)));
		req.on("error", reject);
	});
}

function send(res: http.ServerResponse, status: number, body: string, type = "text/plain; charset=utf-8"): void {
	const data = Buffer.from(body);
	res.writeHead(status, { "Content-Type": type, "Content-Length": data.length });
	res.end(data);
}

function json(res: http.ServerResponse, value: unknown, status = 200): void {
	send(res, status, JSON.stringify(value), "application/json");
}

function need(lookup: Lookup): Project {
	if ("project" in lookup) return lookup.project;
	throw new HttpError(lookup.status, lookup.message);
}

async function handle(req: http.IncomingMessage, res: http.ServerResponse): Promise<void> {
	// Browsers always send Origin on cross-site requests; Studio and the CLI never do.
	// That, and the Host check (DNS rebinding), keep web pages from writing into a repo.
	const host = (req.headers.host ?? "").replace(/:\d+$/, "");
	if (req.headers.origin || !["localhost", "127.0.0.1", "[::1]"].includes(host)) {
		throw new HttpError(403, "forbidden");
	}
	const url = new URL(req.url ?? "/", "http://localhost");
	const q = url.searchParams;
	const route = url.pathname;

	if (req.method === "GET") {
		if (route === "/status") return json(res, status());
		if (route === "/plugin/sync.luau") {
			return send(res, 200, fs.readFileSync(path.join(PLUGIN_SOURCE_DIR, "Sync.luau"), "utf8"));
		}
		if (route.startsWith("/tools/") && route.endsWith(".luau") && !route.includes("..")) {
			// files under a project's tools/, for scripts run in Studio
			const lookup = byPlace(q.get("placeId"), q.get("session"));
			const candidates = "project" in lookup ? [lookup.project] : [...projects.values()];
			for (const candidate of candidates) {
				const full = path.join(candidate.dir, route.slice(1));
				if (fs.existsSync(full)) return send(res, 200, fs.readFileSync(full, "utf8"));
			}
			throw new HttpError(404, "no such file");
		}
		if (route === "/wait") {
			const project = byDir(q.get("dir"));
			if (!project) throw new HttpError(404, `no registered project contains ${q.get("dir")}`);
			const result = await project.waitApplied(Number(q.get("timeout") ?? 30) * 1000);
			return json(res, { ...result, version: project.version, error: project.error, project: project.name });
		}
		const project = need(byPlace(q.get("placeId"), q.get("session")));
		if (project.version === 0) throw new HttpError(503, project.error ?? "project not generated yet");
		const client = q.get("client");
		const placeId = Number(q.get("placeId"));
		if (route === "/project") return json(res, project.info());
		if (route === "/tree") {
			if (client) project.touchClient(client, placeId, 0, q.get("plugin") ?? "?", q.get("session"));
			return json(res, project.tree());
		}
		if (route === "/changes") {
			const since = Number(q.get("since") ?? -1);
			const c = client ? project.touchClient(client, placeId, since, q.get("plugin") ?? "?", q.get("session")) : null;
			if (c) c.polling += 1;
			try {
				const wait = Math.min(Number(q.get("wait") ?? LONG_POLL_MS / 1000), 30) * 1000;
				return json(res, await project.since(q.get("epoch") ?? "", since, wait, client ?? ""));
			} finally {
				if (c) {
					c.polling -= 1;
					c.lastSeen = Date.now();
				}
			}
		}
		if (route === "/snapshot/plan") return json(res, plan(project));
		if (route === "/model") {
			const file = q.get("file") ?? "";
			const owned = [...project.places.values()].some((p) => p.file === file && /\.rbxmx?$/.test(file));
			if (!owned) throw new HttpError(404, "not a model file of this project");
			return send(res, 200, fs.readFileSync(path.join(project.dir, file)).toString("base64"));
		}
		throw new HttpError(404, "unknown");
	}

	if (req.method !== "POST") throw new HttpError(405, "GET or POST");
	const body = await readBody(req);

	if (route === "/pull/request" || route === "/snapshot/request") {
		// from the CLI
		const project = byDir(q.get("dir"));
		if (!project) throw new HttpError(404, `no registered project contains ${q.get("dir")}`);
		const result =
			route === "/pull/request"
				? await runCommand(project, { kind: "pull", path: JSON.parse(q.get("path") ?? "[]"), file: q.get("file") || undefined }, 60_000, q.get("from"))
				: await runCommand(project, { kind: "snapshot" }, 600_000, q.get("from"));
		return json(res, result, result.ok ? 200 : 422);
	}
	if (route === "/command/result") {
		settle(q.get("id") ?? "", q.get("error") ? { ok: false, error: q.get("error")! } : JSON.parse(body.toString("utf8") || "{}"));
		return send(res, 200, "ok");
	}

	const project = need(byPlace(q.get("placeId"), q.get("session")));

	if (route === "/writeback") {
		const { ops } = JSON.parse(body.toString("utf8")) as { ops: Op[] };
		return json(res, { results: new Writer(project).apply(ops) });
	}
	if (route === "/backup") {
		const { items } = JSON.parse(body.toString("utf8"));
		return json(res, { dir: saveBackup(project, items) });
	}
	if (route === "/report") {
		const report = JSON.parse(body.toString("utf8"));
		const client = project.clients.get(q.get("client") ?? "");
		if (client) {
			client.errors = report.errors ?? [];
			client.errorsVersion = report.version ?? 0;
			client.unsaved = report.unsaved ?? [];
		}
		for (const error of report.errors ?? []) log(`[${project.name}] Studio: ${error}`);
		return send(res, 200, "ok");
	}
	if (route === "/pull") {
		// from Studio: the serialized instance, base64, in parts of under 1 MB
		const id = q.get("id") ?? "";
		const token = `${project.dir}|${q.get("upload")}`;
		if (q.get("error")) {
			settle(id, { ok: false, error: q.get("error")! });
			return send(res, 200, "ok");
		}
		const parts = pullParts.get(token) ?? [];
		parts.push(body.toString("utf8"));
		pullParts.set(token, parts);
		if (q.get("last") !== "1") return send(res, 200, "part");
		pullParts.delete(token);
		const instancePath: string[] = JSON.parse(q.get("path") ?? "[]");
		try {
			const file = new Writer(project).savePulled(
				instancePath,
				q.get("file") || undefined,
				Buffer.from(parts.join(""), "base64"),
			);
			settle(id, { ok: true, file });
			return json(res, { ok: true, file });
		} catch (e) {
			const error = e instanceof Error ? e.message : String(e);
			settle(id, { ok: false, error });
			return json(res, { ok: false, error }, 422);
		}
	}

	if (route === "/snapshot/file") {
		receiveFile(project, q.get("upload") ?? "", JSON.parse(q.get("path") ?? "[]"), body.toString("utf8"), q.get("last") === "1");
		return send(res, 200, "ok");
	}
	if (route === "/snapshot/commit") {
		const id = q.get("id") ?? "";
		try {
			const report = JSON.parse(body.toString("utf8"));
			const dir = commit(project, q.get("upload") ?? "", report);
			const result = {
				ok: true,
				dir,
				entries: report.entries.length,
				skipped: report.skipped,
				duplicates: report.duplicates,
				top: report.entries.map((e: { path: string[] }) => e.path.join(".")),
			};
			settle(id, result);
			return json(res, result);
		} catch (e) {
			const error = e instanceof Error ? e.message : String(e);
			settle(id, { ok: false, error });
			return json(res, { ok: false, error }, 422);
		}
	}

	// -- the bake receiver (content in studio-sync.json) ---------------------------
	if (route.startsWith("/content/")) {
		const cfg = project.config.content;
		if (!cfg) throw new HttpError(400, 'this project has no "content" in studio-sync.json');
		if (route === "/content/begin") {
			fs.rmSync(project.staging, { recursive: true, force: true });
			fs.mkdirSync(project.staging, { recursive: true });
			return send(res, 200, "ready");
		}
		const match = CONTENT_FILE.exec(route);
		if (match) {
			if (!fs.existsSync(project.staging)) throw new HttpError(409, "POST /content/begin first");
			const [, service, name] = match;
			const folder = path.join(project.staging, service);
			fs.mkdirSync(folder, { recursive: true });
			const data = Buffer.from(body.toString("utf8"), "base64");
			const part = q.get("part");
			// part 1 starts the file, later parts append in order
			const file = path.join(folder, `${name}.rbxm`);
			if (part === null || part === "1") fs.writeFileSync(file, data);
			else fs.appendFileSync(file, data);
			return send(res, 200, String(data.length));
		}
		if (route === "/content/commit") {
			if (!fs.existsSync(project.staging)) throw new HttpError(409, "POST /content/begin first");
			const hashes: Record<string, string> = {};
			for (const source of [...(cfg.sources ?? [])].sort()) {
				hashes[source] = crypto.createHash("sha256").update(fs.readFileSync(path.join(project.dir, source))).digest("hex");
			}
			fs.writeFileSync(path.join(project.staging, "bake.json"), JSON.stringify({ sources: hashes }, null, 2) + "\n");
			const target = path.join(project.dir, cfg.dir);
			fs.rmSync(target, { recursive: true, force: true });
			fs.mkdirSync(path.dirname(target), { recursive: true });
			fs.cpSync(project.staging, target, { recursive: true });
			fs.rmSync(project.staging, { recursive: true, force: true });
			const files: string[] = [];
			const walk = (dir: string) => {
				for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
					const full = path.join(dir, entry.name);
					if (entry.isDirectory()) walk(full);
					else files.push(path.relative(project.dir, full).split(path.sep).join("/"));
				}
			};
			walk(target);
			return send(res, 200, files.sort().join("\n"));
		}
	}
	throw new HttpError(404, "unknown");
}

export function serve(): void {
	fs.mkdirSync(STATE_DIR, { recursive: true });
	if (!process.stdout.isTTY) logToFile();
	refreshRegistry();
	setInterval(() => {
		try {
			refreshRegistry();
		} catch (e) {
			log(`registry: ${e}`);
		}
	}, 1000);
	const server = http.createServer((req, res) => {
		handle(req, res).catch((e) => {
			const statusCode = e instanceof HttpError ? e.status : 500;
			if (statusCode === 500) log(`${req.method} ${req.url}: ${e?.stack ?? e}`);
			if (!res.headersSent) send(res, statusCode, e instanceof Error ? e.message : String(e));
		});
	});
	server.keepAliveTimeout = 65_000;
	server.requestTimeout = 0;
	server.on("error", (e: NodeJS.ErrnoException) => {
		if (e.code === "EADDRINUSE") {
			log(`port ${PORT} is in use: another studio-sync (or the old Python one) is running. \`studio-sync restart\`.`);
			process.exit(1);
		}
		throw e;
	});
	server.listen(PORT, "127.0.0.1", () => log(`studio-sync ${VERSION} serving on http://localhost:${PORT} (pid ${process.pid})`));
}

