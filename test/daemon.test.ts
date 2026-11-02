// End to end against a real daemon on a spare port, with a throwaway project
// and a fake plugin speaking HTTP. Needs `rojo` on PATH.

import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { after, before, test } from "node:test";
import { fileURLToPath } from "node:url";

const PORT = 34990 + Math.floor(Math.random() * 8);
const BASE = `http://127.0.0.1:${PORT}`;
const PLACE = 4242;
const ROOT = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "studio-sync-test-"));
const home = path.join(tmp, "home");
const project = path.join(tmp, "game");
let daemon: ChildProcess;
const client = "test-client";

function write(rel: string, text: string) {
	fs.mkdirSync(path.dirname(path.join(project, rel)), { recursive: true });
	fs.writeFileSync(path.join(project, rel), text);
}

async function get(route: string): Promise<any> {
	const sep = route.includes("?") ? "&" : "?";
	const r = await fetch(`${BASE}${route}${sep}placeId=${PLACE}&client=${client}&plugin=test`);
	assert.equal(r.status, 200, await r.clone().text());
	return r.json();
}

async function post(route: string, body: unknown): Promise<any> {
	const sep = route.includes("?") ? "&" : "?";
	const r = await fetch(`${BASE}${route}${sep}placeId=${PLACE}&client=${client}`, { method: "POST", body: JSON.stringify(body) });
	return r.json();
}

async function until<T>(fn: () => Promise<T | undefined>, ms = 8000): Promise<T> {
	const end = Date.now() + ms;
	for (;;) {
		const v = await fn().catch(() => undefined);
		if (v !== undefined) return v;
		if (Date.now() > end) throw new Error("timed out");
		await new Promise((r) => setTimeout(r, 150));
	}
}

let tree: any;
const node = (name: string) => tree.nodes.find((n: any) => n.path.join(".") === name);

before(async () => {
	fs.copyFileSync(path.join(ROOT, "rokit.toml"), path.join(tmp, "rokit.toml"));
	write(
		"default.project.json",
		JSON.stringify({
			name: "game",
			tree: {
				$className: "DataModel",
				ReplicatedStorage: { shared: { $path: "src/shared" }, Packages: { $path: "Packages" } },
				ServerScriptService: { server: { $path: "src/server" } },
			},
		}),
	);
	write("studio-sync.json", JSON.stringify({ placeIds: [PLACE] }));
	write("src/shared/a.luau", "return 1\n");
	write("src/shared/mod/init.luau", "return {}\n");
	write("src/shared/mod/child.luau", "return 2\n");
	write("src/shared/data.json", '{"x":[1,2],"y":{"z":"s"}}');
	write("src/shared/note.txt", "hello");
	write("src/shared/thing.model.json", JSON.stringify({ className: "Folder", children: [{ name: "P", className: "Part", properties: { Anchored: true } }] }));
	write("src/server/main.server.luau", "print(1)\n");
	write("Packages/lib.luau", "return 'lib'\n");
	fs.mkdirSync(home);
	fs.writeFileSync(path.join(home, "projects.json"), JSON.stringify({ projects: [project] }));
	daemon = spawn(process.execPath, [path.join(ROOT, "src/cli.ts"), "serve"], {
		env: { ...process.env, STUDIO_SYNC_HOME: home, STUDIO_SYNC_PORT: String(PORT) },
		cwd: tmp,
		stdio: "inherit",
	});
	tree = await until(async () => {
		const r = await fetch(`${BASE}/tree?placeId=${PLACE}&client=${client}`);
		return r.status === 200 ? r.json() : undefined;
	}, 20000);
});

after(async () => {
	if (daemon && daemon.exitCode === null) {
		const exited = new Promise((resolve) => daemon.once("exit", resolve));
		daemon.kill();
		await exited;
	}
	// Windows holds a killed process's files a moment longer
	fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
});

test("the tree carries every file kind", () => {
	assert.deepEqual(tree.roots, [["ReplicatedStorage", "shared"], ["ReplicatedStorage", "Packages"], ["ServerScriptService", "server"]]);
	assert.equal(node("ReplicatedStorage.shared.a").kind, "script");
	assert.equal(node("ReplicatedStorage.shared.a").source, "return 1\n");
	assert.equal(node("ReplicatedStorage.shared.mod").kind, "init");
	assert.equal(node("ReplicatedStorage.shared.data").kind, "generated");
	assert.match(node("ReplicatedStorage.shared.data").source, /x = \{\n\t\t1,/);
	assert.equal(node("ReplicatedStorage.shared.note").properties.Value, "hello");
	assert.equal(node("ReplicatedStorage.shared.thing.P").properties.Anchored, true);
	assert.equal(node("ServerScriptService.server.main").runContext, "Server");
	assert.equal(node("ReplicatedStorage.Packages.lib").ro, true, "Packages are read-only by default");
	assert.equal(node("ReplicatedStorage.shared").kind, "dir");
});

test("browsers are refused", async () => {
	const r = await fetch(`${BASE}/status`, { headers: { Origin: "https://evil.example" } });
	assert.equal(r.status, 403);
});

test("a disk edit arrives as a delta and wait sees Studio apply it", async () => {
	write("src/shared/a.luau", "return 10\n");
	const delta = await get(`/changes?epoch=${tree.epoch}&since=${tree.version}&wait=10`);
	assert.equal(delta.full, false);
	assert.deepEqual(delta.upserts.map((n: any) => n.path.join(".")), ["ReplicatedStorage.shared.a"]);
	// the plugin acks by polling again with the new version
	const ack = get(`/changes?epoch=${tree.epoch}&since=${delta.version}&wait=2`);
	const waited = await (await fetch(`${BASE}/wait?dir=${encodeURIComponent(project)}&timeout=5`)).json();
	assert.equal(waited.ok, true);
	await ack;
	tree = await get("/tree");
});

test("Studio source edits are written, stale ones are conflicts", async () => {
	const a = node("ReplicatedStorage.shared.a");
	const ok = await post("/writeback", { ops: [{ op: "source", path: a.path, source: "return 11\n", base: a.sourceHash }] });
	assert.equal(ok.results[0].ok, true);
	assert.equal(fs.readFileSync(path.join(project, "src/shared/a.luau"), "utf8"), "return 11\n");
	const stale = await post("/writeback", { ops: [{ op: "source", path: a.path, source: "return 12\n", base: a.sourceHash }] });
	assert.equal(stale.results[0].ok, false);
	assert.equal(stale.results[0].conflict.source, "return 11\n");
	assert.ok(fs.existsSync(stale.results[0].conflict.saved));
	assert.equal(fs.readFileSync(path.join(project, "src/shared/a.luau"), "utf8"), "return 11\n", "disk wins");
});

test("create, nest under a single-file module, rename, delete", async () => {
	const shared = ["ReplicatedStorage", "shared"];
	let r = await post("/writeback", {
		ops: [
			{ op: "create", path: [...shared, "ui"], className: "Folder" },
			{ op: "create", path: [...shared, "ui", "button"], className: "ModuleScript", source: "return 'b'\n" },
			{ op: "create", path: [...shared, "ui", "boot"], className: "LocalScript", source: "print('c')\n" },
			{ op: "create", path: [...shared, "a", "inner"], className: "ModuleScript", source: "return 'i'\n" },
		],
	});
	assert.deepEqual(r.results.map((x: any) => x.ok), [true, true, true, true], JSON.stringify(r));
	assert.ok(fs.existsSync(path.join(project, "src/shared/ui/button.luau")));
	assert.ok(fs.existsSync(path.join(project, "src/shared/ui/boot.client.luau")));
	assert.equal(fs.readFileSync(path.join(project, "src/shared/a/init.luau"), "utf8"), "return 11\n");
	assert.ok(fs.existsSync(path.join(project, "src/shared/a/inner.luau")));

	r = await post("/writeback", {
		ops: [
			{ op: "move", from: [...shared, "ui", "button"], to: [...shared, "ui", "Button2"] },
			{ op: "move", from: [...shared, "ui"], to: [...shared, "hud"] },
		],
	});
	assert.deepEqual(r.results.map((x: any) => x.ok), [true, true], JSON.stringify(r));
	assert.ok(fs.existsSync(path.join(project, "src/shared/hud/Button2.luau")));

	tree = await until(async () => {
		const t = await get("/tree");
		return t.nodes.some((n: any) => n.path.join(".") === "ReplicatedStorage.shared.hud.Button2") ? t : undefined;
	});
	r = await post("/writeback", { ops: [{ op: "delete", path: [...shared, "hud"] }] });
	assert.equal(r.results[0].ok, true);
	assert.ok(!fs.existsSync(path.join(project, "src/shared/hud")));
	assert.ok(fs.readdirSync(path.join(home, "backups", "game")).some((d) => d.endsWith("-deleted")));
});

test("refusals: read-only, generated, bad names, project roots", async () => {
	const r = await post("/writeback", {
		ops: [
			{ op: "source", path: ["ReplicatedStorage", "Packages", "lib"], source: "x", base: null },
			{ op: "source", path: ["ReplicatedStorage", "shared", "data"], source: "x", base: null },
			{ op: "create", path: ["ReplicatedStorage", "shared", "x.server"], className: "ModuleScript", source: "" },
			{ op: "create", path: ["ReplicatedStorage", "shared", "A"], className: "ModuleScript", source: "" },
			{ op: "delete", path: ["ReplicatedStorage", "shared"] },
		],
	});
	assert.deepEqual(r.results.map((x: any) => x.ok), [false, false, false, false, false], JSON.stringify(r));
	assert.match(r.results[3].error, /already exists/, "case-insensitive clash with a/");
});

test("pull saves a serialized instance as .rbxm", async () => {
	const pulled = fetch(
		`${BASE}/pull/request?dir=${encodeURIComponent(project)}&path=${encodeURIComponent(JSON.stringify(["ReplicatedStorage", "shared", "Tree"]))}`,
		{ method: "POST" },
	);
	// the fake plugin: keep polling until the command shows up, then upload
	tree = await get("/tree");
	const delta = await until(async () => {
		const d = await get(`/changes?epoch=${tree.epoch}&since=${tree.version}&wait=1`);
		return d.commands?.length ? d : undefined;
	});
	const command = delta.commands[0];
	const data = Buffer.from("fake rbxm bytes").toString("base64");
	const q = `id=${command.id}&upload=u1&path=${encodeURIComponent(JSON.stringify(command.path))}&file=`;
	await fetch(`${BASE}/pull?placeId=${PLACE}&${q}&part=1&last=0`, { method: "POST", body: data.slice(0, 8) });
	await fetch(`${BASE}/pull?placeId=${PLACE}&${q}&part=2&last=1`, { method: "POST", body: data.slice(8) });
	const result = await (await pulled).json();
	assert.equal(result.ok, true, JSON.stringify(result));
	assert.equal(fs.readFileSync(path.join(project, "src/shared/Tree.rbxm"), "utf8"), "fake rbxm bytes");
});

test("a snapshot replaces the snapshot dir but keeps base.rbxl", async () => {
	write("place/base.rbxl", "base place bytes");
	write("place/Workspace/Old.rbxm", "stale");
	const snapshotted = fetch(`${BASE}/snapshot/request?dir=${encodeURIComponent(project)}`, { method: "POST" });
	tree = await get("/tree");
	const delta = await until(async () => {
		const d = await get(`/changes?epoch=${tree.epoch}&since=${tree.version}&wait=1`);
		return d.commands?.length ? d : undefined;
	});
	const command = delta.commands[0];
	assert.equal(command.kind, "snapshot");
	const map = encodeURIComponent(JSON.stringify(["Workspace", "Map"]));
	await fetch(`${BASE}/snapshot/file?placeId=${PLACE}&upload=s1&path=${map}&last=1`, {
		method: "POST",
		body: Buffer.from("map bytes").toString("base64"),
	});
	const report = { placeId: PLACE, entries: [{ path: ["Workspace", "Map"], className: "Model" }], services: {}, skipped: [], duplicates: [] };
	await fetch(`${BASE}/snapshot/commit?placeId=${PLACE}&upload=s1&id=${command.id}`, { method: "POST", body: JSON.stringify(report) });
	const result = await (await snapshotted).json();
	assert.equal(result.ok, true, JSON.stringify(result));
	assert.equal(fs.readFileSync(path.join(project, "place/Workspace/Map.rbxm"), "utf8"), "map bytes");
	assert.ok(!fs.existsSync(path.join(project, "place/Workspace/Old.rbxm")), "the rest of the old snapshot is replaced");
	assert.equal(fs.readFileSync(path.join(project, "place/base.rbxl"), "utf8"), "base place bytes");
});

test("with several Studios connected, a command goes only where --from says", async () => {
	const session = "c0ffee00";
	const registry = JSON.parse(fs.readFileSync(path.join(home, "projects.json"), "utf8"));
	registry.sessions = { [session]: { dir: project, file: path.join(tmp, "copy.rbxl"), createdAt: new Date().toISOString() } };
	fs.writeFileSync(path.join(home, "projects.json"), JSON.stringify(registry));
	const asCopy = (route: string) => `${BASE}${route}${route.includes("?") ? "&" : "?"}placeId=0&client=copy-client&plugin=test&session=${session}`;
	const copyTree = await until(async () => {
		const r = await fetch(asCopy("/tree"));
		return r.status === 200 ? r.json() : undefined;
	});
	await get("/tree"); // the place's Studio is live too

	const request = (from?: string) => {
		const query = new URLSearchParams({ dir: project, path: JSON.stringify(["Workspace", "Thing"]), ...(from ? { from } : {}) });
		return fetch(`${BASE}/pull/request?${query}`, { method: "POST" });
	};
	const ambiguous = await request();
	assert.equal(ambiguous.status, 409);
	assert.match(await ambiguous.text(), new RegExp(`copy ${session}.*place ${PLACE}|place ${PLACE}.*copy ${session}`));
	const unknown = await request("nope");
	assert.equal(unknown.status, 409);
	assert.match(await unknown.text(), /no connected Studio is nope/);

	const pulled = request(session);
	const delta = await until(async () => {
		const d = await (await fetch(asCopy(`/changes?epoch=${copyTree.epoch}&since=${copyTree.version}&wait=1`))).json();
		return d.commands?.length ? d : undefined;
	});
	const placeDelta = await get(`/changes?epoch=${tree.epoch}&since=${tree.version}&wait=1`);
	assert.deepEqual(placeDelta.commands ?? [], [], "the place's Studio is not asked");
	await fetch(asCopy(`/pull?id=${delta.commands[0].id}&error=${encodeURIComponent("answered by the copy")}`), { method: "POST" });
	const result = await (await pulled).json();
	assert.equal(result.error, "answered by the copy");
});

test("a closed copy stops counting at once, so the place is the only Studio again", async () => {
	const registry = JSON.parse(fs.readFileSync(path.join(home, "projects.json"), "utf8"));
	registry.sessions = {};
	fs.writeFileSync(path.join(home, "projects.json"), JSON.stringify(registry));
	await until(async () => {
		const status = await get("/status");
		const clients = status.projects.flatMap((p: any) => p.clients);
		return clients.some((c: any) => c.session) ? undefined : true;
	});
	const query = new URLSearchParams({ dir: project, path: JSON.stringify(["Workspace", "Thing"]) });
	const pulled = fetch(`${BASE}/pull/request?${query}`, { method: "POST" });
	const delta = await until(async () => {
		const d = await get(`/changes?epoch=${tree.epoch}&since=${tree.version}&wait=1`);
		return d.commands?.length ? d : undefined;
	});
	await fetch(`${BASE}/pull?placeId=${PLACE}&client=${client}&plugin=test&id=${delta.commands[0].id}&error=${encodeURIComponent("answered by the place")}`, { method: "POST" });
	const result = await (await pulled).json();
	assert.equal(result.error, "answered by the place");
});
