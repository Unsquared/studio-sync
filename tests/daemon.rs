// End to end against a real daemon on a spare port, with a throwaway project
// and a fake plugin speaking HTTP and its WebSocket.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::{Message, WebSocket};
use ureq::Agent;

const PLACE: u64 = 4242;
const CLIENT: &str = "test-client";
const ROOT: &str = env!("CARGO_MANIFEST_DIR");

// A fake Studio's socket.
struct Socket(WebSocket<TcpStream>);

impl Socket {
	fn open(base: &str, query: &str) -> Socket {
		Socket::open_at(base, "socket", query)
	}

	fn open_at(base: &str, route: &str, query: &str) -> Socket {
		let address = base.trim_start_matches("http://");
		let stream = TcpStream::connect(address).unwrap();
		let (socket, _) = tokio_tungstenite::tungstenite::client(format!("ws://{address}/{route}?{query}"), stream).unwrap();
		Socket(socket)
	}

	// The next message within `wait`, or None.
	fn next_within(&mut self, wait: Duration) -> Option<Value> {
		self.0.get_ref().set_read_timeout(Some(wait)).unwrap();
		loop {
			match self.0.read() {
				Ok(Message::Text(text)) => return Some(serde_json::from_str(&text).unwrap()),
				Ok(Message::Close(_)) => return None,
				Ok(_) => {}
				Err(_) => return None,
			}
		}
	}

	// The next message of one of these types; others are passed over.
	fn next(&mut self, types: &[&str]) -> Value {
		let end = Instant::now() + Duration::from_secs(20);
		loop {
			let left = end.saturating_duration_since(Instant::now());
			assert!(!left.is_zero(), "no {types:?} message");
			if let Some(message) = self.next_within(left)
				&& types.contains(&message["type"].as_str().unwrap_or_default())
			{
				return message;
			}
		}
	}

	fn send(&mut self, message: Value) {
		self.0.send(Message::text(message.to_string())).unwrap();
	}

	fn close(mut self) {
		self.0.close(None).unwrap();
		while self.0.read().is_ok() {}
	}
}

struct Harness {
	base: String,
	studio: Option<Socket>,
	tmp: PathBuf,
	home: PathBuf,
	project: PathBuf,
	agent: Agent,
	daemon: Child,
	tree: Value,
}

impl Drop for Harness {
	fn drop(&mut self) {
		let _ = self.daemon.kill();
		let _ = self.daemon.wait();
		let _ = fs::remove_dir_all(&self.tmp);
	}
}

fn encode(text: &str) -> String {
	text.bytes()
		.map(|b| match b {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
			_ => format!("%{b:02X}"),
		})
		.collect()
}

fn sep(route: &str) -> &'static str {
	if route.contains('?') { "&" } else { "?" }
}

impl Harness {
	fn write(&self, rel: &str, text: &str) {
		let file = self.project.join(rel);
		fs::create_dir_all(file.parent().unwrap()).unwrap();
		fs::write(file, text).unwrap();
	}

	fn read(&self, rel: &str) -> String {
		fs::read_to_string(self.project.join(rel)).unwrap()
	}

	fn exists(&self, rel: &str) -> bool {
		self.project.join(rel).exists()
	}

	fn raw_get(&self, url: &str) -> (u16, String) {
		let mut r = self.agent.get(url).call().unwrap();
		(r.status().as_u16(), r.body_mut().read_to_string().unwrap())
	}

	fn raw_post(&self, url: &str, body: &str) -> (u16, String) {
		let mut r = self.agent.post(url).send(body).unwrap();
		(r.status().as_u16(), r.body_mut().read_to_string().unwrap())
	}

	// As the place's plugin.
	fn get(&self, route: &str) -> Value {
		let (status, body) = self.raw_get(&format!("{}{route}{}placeId={PLACE}&client={CLIENT}&plugin=test", self.base, sep(route)));
		assert_eq!(status, 200, "{body}");
		serde_json::from_str(&body).unwrap()
	}

	fn node(&self, name: &str) -> Value {
		let nodes = self.tree["nodes"].as_array().unwrap();
		nodes.iter().find(|n| joined(&n["path"]) == name).cloned().unwrap_or_else(|| panic!("no node {name}"))
	}

	fn studio(&mut self) -> &mut Socket {
		self.studio.as_mut().expect("the place's socket")
	}

	// A command for the place's plugin, once it is sent.
	fn next_command(&mut self) -> Value {
		self.studio().next(&["pull", "snapshot"])
	}

	fn registry(&self) -> Value {
		serde_json::from_str(&fs::read_to_string(self.home.join("projects.json")).unwrap()).unwrap()
	}

	fn set_registry(&self, registry: &Value) {
		fs::write(self.home.join("projects.json"), registry.to_string()).unwrap();
	}

	fn dir_query(&self) -> String {
		format!("dir={}", encode(&self.project.to_string_lossy()))
	}
}

fn joined(path: &Value) -> String {
	path.as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect::<Vec<_>>().join(".")
}

fn until<T>(mut f: impl FnMut() -> Option<T>) -> T {
	let end = Instant::now() + Duration::from_secs(20);
	loop {
		if let Some(v) = f() {
			return v;
		}
		assert!(Instant::now() < end, "timed out");
		sleep(Duration::from_millis(150));
	}
}

// A background request (the CLI's long calls), answered on join.
fn spawn_post(agent: &Agent, url: String) -> std::thread::JoinHandle<(u16, Value)> {
	let agent = agent.clone();
	std::thread::spawn(move || {
		let mut r = agent.post(&url).send_empty().unwrap();
		let status = r.status().as_u16();
		(status, serde_json::from_str(&r.body_mut().read_to_string().unwrap()).unwrap())
	})
}

fn start() -> Harness {
	let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
	let port = 34990 + (nanos % 8) as u16;
	let tmp = std::env::temp_dir().join(format!("studio-sync-test-{nanos}"));
	let home = tmp.join("home");
	let project = tmp.join("game");
	fs::create_dir_all(&home).unwrap();
	fs::create_dir_all(&project).unwrap();
	// as `add` registers it, and as a CLI's current directory reads (macOS: /var is a link)
	let (tmp, home, project) = (fs::canonicalize(&tmp).unwrap(), fs::canonicalize(&home).unwrap(), fs::canonicalize(&project).unwrap());
	fs::copy(Path::new(ROOT).join("rokit.toml"), tmp.join("rokit.toml")).unwrap();
	let agent = Agent::new_with_config(Agent::config_builder().http_status_as_error(false).build());
	let mut h = Harness {
		base: format!("http://127.0.0.1:{port}"),
		daemon: Command::new(env!("CARGO_BIN_EXE_studio-sync"))
			.arg("serve")
			.env("STUDIO_SYNC_HOME", &home)
			.env("STUDIO_SYNC_PORT", port.to_string())
			.current_dir(&tmp)
			.spawn()
			.unwrap(),
		tmp,
		home,
		project,
		agent,
		tree: Value::Null,
		studio: None,
	};
	h.write(
		"default.project.json",
		&json!({
			"name": "game",
			"tree": {
				"$className": "DataModel",
				"ReplicatedStorage": { "shared": { "$path": "src/shared" }, "Packages": { "$path": "Packages" } },
				"ServerScriptService": { "server": { "$path": "src/server" } },
			},
		})
		.to_string(),
	);
	h.write("studio-sync.json", &json!({ "placeIds": [PLACE] }).to_string());
	h.write("src/shared/a.luau", "return 1\n");
	h.write("src/shared/mod/init.luau", "return {}\n");
	h.write("src/shared/mod/child.luau", "return 2\n");
	h.write("src/shared/data.json", r#"{"x":[1,2],"y":{"z":"s"}}"#);
	h.write("src/shared/note.txt", "hello");
	h.write(
		"src/shared/thing.model.json",
		&json!({ "className": "Folder", "children": [{ "name": "P", "className": "Part", "properties": { "Anchored": true } }] })
			.to_string(),
	);
	h.write("src/server/main.server.luau", "print(1)\n");
	h.write("Packages/lib.luau", "return 'lib'\n");
	h.set_registry(&json!({ "projects": [h.project] }));
	h.tree = until(|| {
		let (status, body) = h
			.agent
			.get(format!("{}/tree?placeId={PLACE}&client={CLIENT}", h.base))
			.call()
			.ok()
			.map(|mut r| (r.status().as_u16(), r.body_mut().read_to_string().unwrap_or_default()))?;
		(status == 200).then(|| serde_json::from_str::<Value>(&body).unwrap())
	});
	let query =
		format!("placeId={PLACE}&client={CLIENT}&plugin=test&epoch={}&version={}", h.tree["epoch"].as_str().unwrap(), h.tree["version"]);
	h.studio = Some(Socket::open(&h.base, &query));
	h
}

#[test]
fn end_to_end() {
	let mut h = start();
	tree_carries_every_file_kind(&h);
	browsers_are_refused(&h);
	disk_edit_arrives_and_wait_sees_it(&mut h);
	pull_saves_rbxm(&mut h);
	model_saves(&mut h);
	new_copy_keeps_model_files(&mut h);
	snapshot_replaces_dir(&mut h);
	build_layers_service_settings(&h);
	commands_go_where_from_says(&mut h);
	wait_flushes_model_edits(&mut h);
	resync_is_a_full_sync(&mut h);
	help_and_unknown_flags_do_nothing(&h);
	wait_says_what_it_found(&mut h);
	claim_leaves_another_checkouts_studio_alone(&h);
	status_explains_a_lost_config(&h);
	closed_copies_clean_themselves(&h);
	changes_are_short(&h);
	events_are_written(&h);
}

fn tree_carries_every_file_kind(h: &Harness) {
	assert_eq!(
		h.tree["roots"],
		json!([["ReplicatedStorage", "shared"], ["ReplicatedStorage", "Packages"], ["ServerScriptService", "server"]])
	);
	assert_eq!(h.node("ReplicatedStorage.shared.a")["kind"], "script");
	assert_eq!(h.node("ReplicatedStorage.shared.a")["source"], "return 1\n");
	assert_eq!(h.node("ReplicatedStorage.shared.mod")["kind"], "init");
	assert_eq!(h.node("ReplicatedStorage.shared.data")["kind"], "generated");
	assert!(h.node("ReplicatedStorage.shared.data")["source"].as_str().unwrap().contains("x = {1"));
	assert_eq!(h.node("ReplicatedStorage.shared.note")["properties"]["Value"], "hello");
	assert_eq!(h.node("ReplicatedStorage.shared.thing.P")["properties"]["Anchored"], true);
	assert_eq!(h.node("ServerScriptService.server.main")["runContext"], "Server");
	assert_eq!(h.node("ReplicatedStorage.Packages.lib")["ro"], true, "Packages are read-only by default");
	assert_eq!(h.node("ReplicatedStorage.shared")["kind"], "dir");
	// parents come before their children
	let paths: Vec<String> = h.tree["nodes"].as_array().unwrap().iter().map(|n| joined(&n["path"])).collect();
	for (i, path) in paths.iter().enumerate() {
		if let Some((parent, _)) = path.rsplit_once('.')
			&& let Some(at) = paths.iter().position(|p| p == parent)
		{
			assert!(at < i, "{parent} comes after {path}");
		}
	}
}

fn browsers_are_refused(h: &Harness) {
	for route in ["/status", "/socket"] {
		let r = h.agent.get(format!("{}{route}", h.base)).header("Origin", "https://evil.example").call().unwrap();
		assert_eq!(r.status().as_u16(), 403);
	}
}

fn disk_edit_arrives_and_wait_sees_it(h: &mut Harness) {
	h.write("src/shared/a.luau", "return 10\n");
	let epoch = h.tree["epoch"].as_str().unwrap().to_string();
	let rung = h.studio().next(&["version"]);
	assert_eq!(rung["epoch"], epoch);
	assert_eq!(rung["version"], h.tree["version"].as_u64().unwrap() + 1);
	let delta = h.get(&format!("/changes?epoch={epoch}&since={}", h.tree["version"]));
	assert_eq!(delta["full"], false);
	assert_eq!(delta["version"], rung["version"]);
	let upserts: Vec<String> = delta["upserts"].as_array().unwrap().iter().map(|n| joined(&n["path"])).collect();
	assert_eq!(upserts, ["ReplicatedStorage.shared.a"]);
	assert_eq!(h.get(&format!("/changes?epoch=old&since={}", h.tree["version"]))["full"], true, "another epoch is a full sync");
	// `wait` returns once the plugin says it applied the version
	let waited = {
		let url = format!("{}/wait?{}&timeout=10", h.base, h.dir_query());
		let agent = h.agent.clone();
		std::thread::spawn(move || -> Value {
			serde_json::from_str(&agent.get(&url).call().unwrap().body_mut().read_to_string().unwrap()).unwrap()
		})
	};
	sleep(Duration::from_millis(300));
	assert!(!waited.is_finished(), "wait waits for the plugin");
	h.studio().send(json!({ "type": "applied", "version": delta["version"] }));
	let waited = waited.join().unwrap();
	assert_eq!(waited["ok"], true, "{waited}");
	h.studio().send(json!({ "type": "report", "errors": ["X: broke"] }));
	let client = until(|| {
		let status = h.get("/status");
		let client = status["projects"][0]["clients"][0].clone();
		(client["errors"] == json!(["X: broke"])).then_some(client)
	});
	assert_eq!(client["version"], delta["version"]);
	h.studio().send(json!({ "type": "report", "errors": [] }));
	h.tree = h.get("/tree");
}

fn pull_saves_rbxm(h: &mut Harness) {
	let path = encode(r#"["ReplicatedStorage","shared","Tree"]"#);
	let pulled = spawn_post(&h.agent, format!("{}/pull/request?{}&path={path}", h.base, h.dir_query()));
	// the fake plugin: keep polling until the command shows up, then upload
	let command = h.next_command();
	let data = "ZmFrZSByYnhtIGJ5dGVz"; // "fake rbxm bytes"
	let q = format!("id={}&upload=u1&path={}&file=", command["id"].as_str().unwrap(), encode(&command["path"].to_string()));
	h.raw_post(&format!("{}/pull?placeId={PLACE}&{q}&part=1&last=0", h.base), &data[..8]);
	h.raw_post(&format!("{}/pull?placeId={PLACE}&{q}&part=2&last=1", h.base), &data[8..]);
	let (_, result) = pulled.join().unwrap();
	assert_eq!(result["ok"], true, "{result}");
	assert_eq!(h.read("src/shared/Tree.rbxm"), "fake rbxm bytes");
}

fn snapshot_replaces_dir(h: &mut Harness) {
	h.write("place/Workspace/Old.rbxm", "stale");
	let snapshotted = spawn_post(&h.agent, format!("{}/snapshot/request?{}", h.base, h.dir_query()));
	let command = h.next_command();
	assert_eq!(command["type"], "snapshot");
	let map = encode(r#"["Workspace","Map"]"#);
	h.raw_post(&format!("{}/snapshot/file?placeId={PLACE}&upload=s1&path={map}&last=1", h.base), "bWFwIGJ5dGVz"); // "map bytes"
	let report = json!({ "placeId": PLACE, "entries": [{ "path": ["Workspace", "Map"], "className": "Model" }], "services": {}, "skipped": [], "duplicates": [] });
	h.raw_post(
		&format!("{}/snapshot/commit?placeId={PLACE}&upload=s1&id={}", h.base, command["id"].as_str().unwrap()),
		&report.to_string(),
	);
	let (_, result) = snapshotted.join().unwrap();
	assert_eq!(result["ok"], true, "{result}");
	assert_eq!(h.read("place/Workspace/Map.rbxm"), "map bytes");
	assert!(!h.exists("place/Workspace/Old.rbxm"), "the rest of the old snapshot is replaced");
	let backups = fs::read_dir(h.home.join("backups").join("game")).unwrap();
	let old =
		backups.map(|e| e.unwrap().path()).find(|d| d.to_string_lossy().ends_with("-snapshot")).expect("a backup of the old snapshot");
	assert_eq!(fs::read_to_string(old.join("Workspace").join("Old.rbxm")).unwrap(), "stale");
}

// Models: an edit saves over the file (backing up a file it did not come from); a connect
// compares both sides with the version they last agreed on, and the side that changed wins.
fn model_saves(h: &mut Harness) {
	let tree = until(|| {
		let node =
			h.get("/tree")["nodes"].as_array().unwrap().iter().find(|n| joined(&n["path"]) == "ReplicatedStorage.shared.Tree").cloned();
		node.filter(|n| n["kind"] == "model")
	});
	let path = encode(&tree["path"].to_string());
	let send = |mode: &str, base: &str, text: &str| -> Value {
		let data = BASE64.encode(text);
		let route = format!("{}/save?placeId={PLACE}&upload={mode}{base}&mode={mode}&base={base}&path={path}", h.base);
		h.raw_post(&format!("{route}&part=1&last=0"), &data[..4]);
		serde_json::from_str(&h.raw_post(&format!("{route}&part=2&last=1"), &data[4..]).1).unwrap()
	};
	let hash = |text: &str| Sha1::digest(text)[..8].iter().map(|b| format!("{b:02x}")).collect::<String>();
	let file = "src/shared/Tree.rbxm";

	// an edit of what Studio loaded: saved, nothing backed up
	let base = tree["modelHash"].as_str().unwrap().to_string();
	let r = send("edit", &base, "v1");
	assert_eq!((r["action"].as_str(), r["replaced"].is_null()), (Some("saved"), true), "{r}");
	assert_eq!(h.read(file), "v1");
	// an edit from an older file: Studio wins, the newer file goes to backups
	let r = send("edit", &base, "v2");
	assert_eq!(fs::read_to_string(Path::new(r["replaced"].as_str().unwrap()).join(file)).unwrap(), "v1", "{r}");
	assert_eq!(h.read(file), "v2");

	// connect, nothing changed since the last save: nothing to do
	assert_eq!(send("connect", "", "v2")["action"], "none");
	// the file changed (a git pull), Studio did not: load it
	h.write(file, "pulled");
	assert_eq!(send("connect", "", "v2")["action"], "load");
	assert_eq!(h.read(file), "pulled");
	// Studio loads it; its serialization differs (new ScriptGuids) and is recorded
	assert_eq!(send("loaded", &hash("pulled"), "pulled, as Studio has it")["action"], "recorded");
	assert_eq!(send("connect", "", "pulled, as Studio has it")["action"], "none", "no rewrite for ScriptGuids");
	// Studio changed offline, the file did not: Studio's version saves
	let r = send("connect", "", "studio offline edit");
	assert_eq!((r["action"].as_str(), r["replaced"].is_null()), (Some("saved"), true), "{r}");
	// both changed: Studio wins, the file goes to backups
	h.write(file, "pulled again");
	let r = send("connect", "", "studio edit again");
	assert_eq!(r["action"], "saved");
	assert_eq!(fs::read_to_string(Path::new(r["replaced"].as_str().unwrap()).join(file)).unwrap(), "pulled again");
	assert_eq!(h.read(file), "studio edit again");

	let script = encode(r#"["ReplicatedStorage","shared","a"]"#);
	let refused: Value = serde_json::from_str(
		&h.raw_post(&format!("{}/save?placeId={PLACE}&upload=m9&mode=edit&base=x&path={script}&last=1", h.base), "eA==").1,
	)
	.unwrap();
	assert!(refused["error"].as_str().unwrap().contains("not a model file"), "{refused}");
}

// Readable settings come from the snapshot, hidden ones from the project file, which wins.
fn build_layers_service_settings(h: &Harness) {
	let manifest = json!({
		"createdAt": "2026-01-01T00:00:00.000Z",
		"placeId": PLACE,
		"entries": [],
		"services": {
			"Lighting": { "className": "Lighting", "properties": { "Brightness": { "Float32": 2.5 } } },
			"Workspace": { "className": "Workspace", "properties": { "Gravity": { "Float32": 100 } } },
		},
		"skipped": [],
		"duplicates": [],
	});
	h.write("place/snapshot.json", &manifest.to_string());
	fs::remove_file(h.project.join("src/shared/Tree.rbxm")).unwrap(); // the pull test's fake bytes are no model a build can read
	let mut project: Value = serde_json::from_str(&h.read("default.project.json")).unwrap();
	project["tree"]["Workspace"] = json!({ "$properties": { "Gravity": 50, "SignalBehavior": "Deferred" } });
	h.write("default.project.json", &project.to_string());
	let out = Command::new(env!("CARGO_BIN_EXE_studio-sync"))
		.args(["build", "-o", "build/place.rbxlx"])
		.current_dir(&h.project)
		.output()
		.unwrap();
	assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
	let xml = h.read("build/place.rbxlx");
	assert!(xml.contains(r#"name="Brightness">2.5<"#), "the snapshot's readable setting");
	assert!(xml.contains(r#"name="Gravity">50<"#), "the project file wins");
	assert!(xml.contains(r#"name="SignalBehavior2">"#), "a setting no plugin can read");
	assert!(xml.contains(h.read("src/shared/a.luau").trim()), "the code");
}

// `wait` lets a Studio save its model edits first. A plugin of another version is not asked.
fn wait_flushes_model_edits(h: &mut Harness) {
	h.tree = h.get("/tree");
	let plugin = env!("CARGO_PKG_VERSION");
	let query =
		format!("placeId={PLACE}&client=flusher&plugin={plugin}&epoch={}&version={}", h.tree["epoch"].as_str().unwrap(), h.tree["version"]);
	let mut studio = Socket::open(&h.base, &query);
	until(|| (h.get("/status")["projects"][0]["clients"].as_array().unwrap().len() == 2).then_some(()));
	let waited = {
		let url = format!("{}/wait?{}&timeout=15", h.base, h.dir_query());
		let agent = h.agent.clone();
		std::thread::spawn(move || -> Value {
			serde_json::from_str(&agent.get(&url).call().unwrap().body_mut().read_to_string().unwrap()).unwrap()
		})
	};
	let flush = studio.next(&["flush"]);
	sleep(Duration::from_millis(300));
	assert!(!waited.is_finished(), "wait waits for the flush");
	// the edit the flush saves changes a file: it is in the version the wait then waits for
	h.write("src/shared/a.luau", "return 'flushed'\n");
	studio.send(json!({ "type": "flushed", "seq": flush["seq"] }));
	let version = until(|| {
		let status = h.get("/status");
		let project = &status["projects"][0];
		(project["clients"][0]["version"] != project["version"]).then(|| project["version"].clone())
	});
	for socket in [&mut studio, h.studio.as_mut().unwrap()] {
		socket.send(json!({ "type": "applied", "version": version }));
	}
	// then it checks what Studio holds
	let verify = studio.next(&["verify"]);
	sleep(Duration::from_millis(200));
	assert!(!waited.is_finished(), "wait waits for the check");
	studio.send(json!({ "type": "verified", "seq": verify["seq"] }));
	let waited = waited.join().unwrap();
	assert_eq!((&waited["ok"], &waited["version"]), (&json!(true), &version), "{waited}");

	let wait = |h: &Harness| {
		let url = format!("{}/wait?{}&timeout=15", h.base, h.dir_query());
		let agent = h.agent.clone();
		std::thread::spawn(move || -> Value {
			serde_json::from_str(&agent.get(&url).call().unwrap().body_mut().read_to_string().unwrap()).unwrap()
		})
	};
	let flushed = |studio: &mut Socket| {
		let flush = studio.next(&["flush"]);
		studio.send(json!({ "type": "flushed", "seq": flush["seq"] }));
	};

	// a Studio that lost what the sync made in it repairs itself, and the wait lasts until it has
	let waited = wait(h);
	flushed(&mut studio);
	let verify = studio.next(&["verify"]);
	studio.send(json!({ "type": "verified", "seq": verify["seq"], "missing": ["ReplicatedStorage.shared.a"], "edited": [] }));
	sleep(Duration::from_millis(300));
	assert!(!waited.is_finished(), "wait waits for the full sync");
	studio.send(json!({ "type": "applied", "version": version }));
	let verify = studio.next(&["verify"]);
	studio.send(json!({ "type": "verified", "seq": verify["seq"], "missing": [], "edited": [] }));
	let waited = waited.join().unwrap();
	assert_eq!(waited["ok"], true, "{waited}");
	assert!(waited["clients"].as_array().unwrap().iter().all(|c| c["missing"] == json!([])), "{waited}");

	// what a full sync could not bring back, and script text that differs, are in the answer
	let waited = wait(h);
	flushed(&mut studio);
	let verify = studio.next(&["verify"]);
	studio.send(json!({ "type": "verified", "seq": verify["seq"], "missing": ["A.gone"], "edited": ["A.b"] }));
	studio.send(json!({ "type": "applied", "version": version }));
	let verify = studio.next(&["verify"]);
	studio.send(json!({ "type": "verified", "seq": verify["seq"], "missing": ["A.gone"], "edited": ["A.b"] }));
	let waited = waited.join().unwrap();
	let client = waited["clients"].as_array().unwrap().iter().find(|c| c["missing"] != json!([])).cloned().unwrap();
	assert_eq!((&client["missing"], &client["edited"]), (&json!(["A.gone"]), &json!(["A.b"])), "{waited}");

	// a model saved from Studio is named by the next wait
	studio.send(json!({ "type": "applied", "version": version }));
	h.write("src/shared/Saved.rbxm", "as built");
	let model = until(|| {
		h.get("/tree")["nodes"]
			.as_array()
			.unwrap()
			.iter()
			.find(|n| joined(&n["path"]) == "ReplicatedStorage.shared.Saved" && n["kind"] == "model")
			.cloned()
	});
	let version = h.get("/status")["projects"][0]["version"].clone();
	let route = format!(
		"{}/save?placeId={PLACE}&upload=seen&mode=edit&base={}&path={}&part=1&last=1",
		h.base,
		model["modelHash"].as_str().unwrap(),
		encode(&model["path"].to_string())
	);
	let saved: Value = serde_json::from_str(&h.raw_post(&route, &BASE64.encode("saved by a test")).1).unwrap();
	assert_eq!(saved["action"], "saved", "{saved}");
	let version = until(|| {
		let status = h.get("/status");
		let project = &status["projects"][0];
		(project["version"] != version).then(|| project["version"].clone())
	});
	for socket in [&mut studio, h.studio.as_mut().unwrap()] {
		socket.send(json!({ "type": "applied", "version": version }));
	}
	let waited = wait(h);
	flushed(&mut studio);
	let verify = studio.next(&["verify"]);
	studio.send(json!({ "type": "verified", "seq": verify["seq"] }));
	let waited = waited.join().unwrap();
	assert_eq!(waited["saved"].as_array().unwrap().len(), 1, "{waited}");

	// a Play that started before this version is named, until it stops
	let play = Socket::open_at(&h.base, "play", &format!("placeId={PLACE}&client=play1"));
	until(|| (h.get("/status")["projects"][0]["plays"].as_array().unwrap().len() == 1).then_some(()));
	assert_eq!(h.get("/status")["projects"][0]["plays"][0]["stale"], false);
	h.write("src/shared/a.luau", "return 'after play'\n");
	let version = until(|| {
		let status = h.get("/status");
		let project = &status["projects"][0];
		(project["version"] != version).then(|| project["version"].clone())
	});
	assert_eq!(h.get("/status")["projects"][0]["plays"][0]["stale"], true);
	for socket in [&mut studio, h.studio.as_mut().unwrap()] {
		socket.send(json!({ "type": "applied", "version": version }));
	}
	play.close();
	until(|| (h.get("/status")["projects"][0]["plays"].as_array().unwrap().is_empty()).then_some(()));
	studio.close();
	until(|| (h.get("/status")["projects"][0]["clients"].as_array().unwrap().len() == 1).then_some(()));
}

fn resync_is_a_full_sync(h: &mut Harness) {
	let tree = h.get("/tree");
	let route = format!("{}/resync?{}", h.base, h.dir_query());
	assert_eq!(h.raw_post(&route, "").0, 200);
	let wanted = tree["version"].as_u64().unwrap() + 1;
	let rung = loop {
		let rung = h.studio().next(&["version"]); // older ones may be queued
		if rung["version"] == wanted {
			break rung;
		}
	};
	assert_ne!(rung["epoch"], tree["epoch"]);
	assert_eq!(h.get(&format!("/changes?epoch={}&since={}", tree["epoch"].as_str().unwrap(), tree["version"]))["full"], true);
	let fresh = h.get("/tree");
	assert_eq!((&fresh["epoch"], &fresh["version"]), (&rung["epoch"], &rung["version"]));
	let version = rung["version"].clone();
	h.studio().send(json!({ "type": "applied", "version": version }));
}

// The CLI, as an agent runs it.
fn cli(h: &Harness, dir: &Path, args: &[&str]) -> (i32, String, String) {
	let output = Command::new(env!("CARGO_BIN_EXE_studio-sync"))
		.args(args)
		.env("STUDIO_SYNC_HOME", &h.home)
		.env("STUDIO_SYNC_PORT", h.base.rsplit(':').next().unwrap())
		.current_dir(dir)
		.output()
		.unwrap();
	(
		output.status.code().unwrap(),
		String::from_utf8_lossy(&output.stdout).into_owned(),
		String::from_utf8_lossy(&output.stderr).into_owned(),
	)
}

fn wait_says_what_it_found(h: &mut Harness) {
	let tree = h.get("/tree");
	let plugin = env!("CARGO_PKG_VERSION");
	let query =
		format!("placeId={PLACE}&client=waiter&plugin={plugin}&epoch={}&version={}", tree["epoch"].as_str().unwrap(), tree["version"]);
	let mut studio = Socket::open(&h.base, &query);
	until(|| (h.get("/status")["projects"][0]["clients"].as_array().unwrap().len() == 2).then_some(()));
	let mut waited = |h: &Harness, answer: Value| {
		let (home, port, project) = (h.home.clone(), h.base.rsplit(':').next().unwrap().to_string(), h.project.clone());
		let run = std::thread::spawn(move || {
			Command::new(env!("CARGO_BIN_EXE_studio-sync"))
				.args(["wait", "--timeout", "10"])
				.env("STUDIO_SYNC_HOME", home)
				.env("STUDIO_SYNC_PORT", port)
				.current_dir(project)
				.output()
				.unwrap()
		});
		let flush = studio.next(&["flush"]);
		studio.send(json!({ "type": "flushed", "seq": flush["seq"] }));
		let verify = studio.next(&["verify"]);
		let mut message = answer;
		message["type"] = json!("verified");
		message["seq"] = verify["seq"].clone();
		studio.send(message);
		let output = run.join().unwrap();
		(
			output.status.code().unwrap(),
			String::from_utf8_lossy(&output.stdout).into_owned(),
			String::from_utf8_lossy(&output.stderr).into_owned(),
		)
	};

	let (code, out, _) = waited(h, json!({}));
	assert_eq!(code, 0, "{out}");
	assert!(out.contains("Studio is in sync (a running Play keeps the code it started with)"), "{out}");

	let (code, _, err) = waited(h, json!({ "edited": ["ReplicatedStorage.shared.a"] }));
	assert_eq!(code, 3, "text that differs fails the wait");
	assert!(err.contains("ReplicatedStorage.shared.a") && err.contains("studio-sync wait --full"), "{err}");

	let play = Socket::open_at(&h.base, "play", &format!("placeId={PLACE}&client=play2"));
	until(|| (h.get("/status")["projects"][0]["plays"].as_array().unwrap().len() == 1).then_some(()));
	let (code, out, _) = waited(h, json!({}));
	assert_eq!(code, 0, "{out}");
	assert!(out.contains("runs this version") && !out.contains("a running Play keeps"), "{out}");
	play.close();
	until(|| (h.get("/status")["projects"][0]["plays"].as_array().unwrap().is_empty()).then_some(()));
	studio.close();
	until(|| (h.get("/status")["projects"][0]["clients"].as_array().unwrap().len() == 1).then_some(()));
}

fn claim_leaves_another_checkouts_studio_alone(h: &Harness) {
	let other = h.tmp.join("other-checkout");
	fs::create_dir_all(&other).unwrap();
	fs::write(other.join("studio-sync.json"), json!({ "placeIds": [PLACE], "roots": [] }).to_string()).unwrap();
	let claims = || h.registry()["claims"].clone();
	let before = claims();
	let (code, _, err) = cli(h, &other, &["open"]);
	assert_eq!(code, 1, "{err}");
	assert!(err.contains("open --copy") && err.contains("--force"), "{err}");
	assert_eq!(claims(), before, "a refused open changes nothing");

	// nor does an update restart the daemon under a connected Studio
	let (code, _, err) = cli(h, &h.project, &["install", "--update"]);
	assert_eq!(code, 1, "{err}");
	assert!(err.contains("--force") && err.contains("game"), "{err}");

	// --force moves the place here, with no new Studio: it waits for the one that is open
	let mut forced = Command::new(env!("CARGO_BIN_EXE_studio-sync"))
		.args(["open", "--force"])
		.env("STUDIO_SYNC_HOME", &h.home)
		.env("STUDIO_SYNC_PORT", h.base.rsplit(':').next().unwrap())
		.current_dir(&other)
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.spawn()
		.unwrap();
	until(|| (claims()[PLACE.to_string()] == json!(other)).then_some(()));
	let clients_here = || {
		let status = h.get("/status");
		let project = status["projects"].as_array().unwrap().iter().find(|p| p["dir"] == json!(h.project)).unwrap().clone();
		project["clients"].as_array().unwrap().len()
	};
	until(|| (clients_here() == 0).then_some(())); // the Studio's socket closes, so it reconnects to the new owner
	forced.kill().unwrap();
	forced.wait().unwrap();
	let mut registry = h.registry();
	registry["claims"] = if before.is_null() { json!({}) } else { before };
	h.set_registry(&registry);

	// a test's binary never touches the real service, plugin or command, whatever else is true
	for args in [&["install"][..], &["install", "--remove"], &["install", "--update", "--force"]] {
		let (code, _, err) = cli(h, &h.project, args);
		assert_eq!(code, 1, "{args:?}: {err}");
		assert!(err.contains("STUDIO_SYNC_HOME"), "{args:?}: {err}");
	}
}

fn status_explains_a_lost_config(h: &Harness) {
	let other = h.tmp.join("other-checkout");
	let (code, _, err) = cli(h, &other, &["add"]);
	assert_eq!(code, 0, "{err}");
	until(|| (h.get("/status")["projects"].as_array().unwrap().len() == 2).then_some(()));
	sleep(Duration::from_millis(300)); // its watcher is up
	fs::remove_file(other.join("studio-sync.json")).unwrap();
	let out = until(|| {
		let (_, out, _) = cli(h, &h.tmp, &["status"]);
		out.contains("no studio-sync.json there any more").then_some(out)
	});
	assert!(out.contains("studio-sync add"), "{out}");
	let (code, out, _) = cli(h, &h.project, &["status"]);
	assert_eq!(code, 0);
	assert!(!out.contains("other-checkout\n"), "inside a project, status shows that project: {out}");
	assert!(!h.registry()["projects"].as_array().unwrap().iter().any(|p| p == &json!(other)), "it unregisters by itself");
}

// A copy someone closed by hand goes to backups; one that may still be starting stays.
fn closed_copies_clean_themselves(h: &Harness) {
	let (old, young) = (h.tmp.join("closed-copy-aaaa.rbxl"), h.tmp.join("young-copy-bbbb.rbxl"));
	fs::write(&old, "saved in Studio").unwrap();
	fs::write(&young, "starting").unwrap();
	let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
	let session = |file: &PathBuf, built: u64| json!({ "dir": h.tmp.join("gone"), "file": file, "createdAt": "", "builtAt": built });
	let mut registry = h.registry();
	registry["sessions"]["aaaa"] = session(&old, 0);
	registry["sessions"]["bbbb"] = session(&young, now);
	h.set_registry(&registry);
	let (code, out, err) = cli(h, &h.project, &["close"]);
	assert_eq!(code, 0, "{err}");
	assert!(out.contains("moved 1 closed copies to backups: aaaa"), "{out}");
	assert!(!old.exists() && young.exists(), "{out}");
	let backups = fs::read_dir(h.home.join("backups").join("gone")).unwrap();
	let backup = backups.flatten().find(|e| e.file_name().to_string_lossy().ends_with("-copy")).unwrap();
	assert_eq!(fs::read_to_string(backup.path().join("closed-copy-aaaa.rbxl")).unwrap(), "saved in Studio");
	let sessions = h.registry()["sessions"].clone();
	assert!(sessions["aaaa"].is_null() && !sessions["bbbb"].is_null(), "{sessions}");
	let (code, _, err) = cli(h, &h.project, &["close", "bbbb"]);
	assert_eq!(code, 0, "{err}");
	assert!(!young.exists());
}

fn changes_are_short(h: &Harness) {
	let (code, out, err) = cli(h, &h.project, &["changes", "--since", "1d"]);
	assert_eq!(code, 0, "{err}");
	assert!(out.lines().count() <= 11 && out.len() < 2400, "{out}");
	assert!(!out.contains("regen") && !out.contains('{'), "{out}");
	let (_, out, _) = cli(h, &h.project, &["changes", "Saved"]);
	assert!(out.contains("saved from Studio") && out.contains("src/shared/Saved.rbxm"), "{out}");
	let (_, out, _) = cli(h, &h.project, &["changes", "--since", "0m"]);
	assert_eq!(out, "nothing in the last 0m\n");
	let (code, _, err) = cli(h, &h.project, &["changes", "--since", "soon"]);
	assert_eq!(code, 1, "{err}");

	// a model against git's version of it
	let model = |tags: &[&str]| {
		let mut list = rbx_dom_weak::types::Tags::new();
		for tag in tags {
			list.push(tag);
		}
		let part = rbx_dom_weak::InstanceBuilder::new("Part").with_name("cell").with_property("Tags", list);
		let dom = rbx_dom_weak::WeakDom::new(rbx_dom_weak::InstanceBuilder::new("DataModel").with_child(part));
		let mut bytes = Vec::new();
		rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
		bytes
	};
	let repo = h.tmp.join("model-repo");
	fs::create_dir_all(&repo).unwrap();
	fs::write(repo.join("studio-sync.json"), "{}").unwrap();
	fs::write(repo.join("m.rbxm"), model(&["Slot"])).unwrap();
	let git = |args: &[&str]| {
		let done = Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).current_dir(&repo).output().unwrap();
		assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
	};
	git(&["init"]);
	git(&["add", "."]);
	git(&["commit", "-m", "m"]);
	let (_, out, err) = cli(h, &repo, &["changes", "m.rbxm"]);
	assert!(out.starts_with("no change since HEAD\n"), "{out} {err}");
	fs::write(repo.join("m.rbxm"), model(&[])).unwrap();
	let (code, out, err) = cli(h, &repo, &["changes", "m.rbxm"]);
	assert_eq!(code, 0, "{err}");
	assert!(out.starts_with("1x Tags -Slot: cell\n"), "{out}");
	let (code, _, err) = cli(h, &repo, &["changes", "m.rbxm", "--rev", "nonsense"]);
	assert_eq!(code, 1);
	assert!(err.contains("git show nonsense"), "{err}");
}

fn events_are_written(h: &Harness) {
	let text = fs::read_to_string(h.home.join("events.jsonl")).unwrap();
	let events: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
	let of = |kind: &str| events.iter().filter(|e| e["ev"] == kind).collect::<Vec<_>>();
	for kind in ["daemon", "regen", "connect", "disconnect", "applied", "wait", "model", "drift", "play", "resync", "cli"] {
		assert!(!of(kind).is_empty(), "no {kind} event");
	}
	let regen = of("regen").into_iter().find(|e| e["upserts"]["first"] == json!(["ReplicatedStorage.shared.a"]));
	assert!(regen.is_some(), "a regen names what changed");
	let failed = of("cli").into_iter().find(|e| e["args"] == json!(["snapshot", "--nope"])).expect("the command that failed");
	assert!(failed["message"].as_str().unwrap().contains("no flag --nope"), "{failed}");
	let (code, out, _) = cli(h, &h.project, &["changes", "--raw", "-n", "3"]);
	assert_eq!(code, 0);
	assert_eq!(out.lines().count(), 3, "{out}");
	assert!(out.lines().all(|line| serde_json::from_str::<Value>(line).is_ok()), "{out}");
}

// A `--help` must not run the command: it once replaced a snapshot.
fn help_and_unknown_flags_do_nothing(h: &Harness) {
	let run = |args: &[&str]| {
		Command::new(env!("CARGO_BIN_EXE_studio-sync"))
			.args(args)
			.env("STUDIO_SYNC_HOME", &h.home)
			.env("STUDIO_SYNC_PORT", h.base.rsplit(':').next().unwrap())
			.current_dir(&h.project)
			.output()
			.unwrap()
	};
	let version_before = h.get("/status")["projects"][0]["version"].clone();
	for args in [["snapshot", "--help"], ["open", "-h"], ["close", "--help"], ["install", "--help"]] {
		let output = run(&args);
		assert!(output.status.success(), "{args:?}");
		assert!(String::from_utf8_lossy(&output.stdout).contains(args[0]), "{args:?}");
	}
	let top = run(&["--help"]);
	let lines = String::from_utf8_lossy(&top.stdout).lines().count();
	assert!(lines <= 16, "the main help stays short: {lines} lines");
	for (gone, instead) in [("claim", "open"), ("log", "--raw"), ("update", "--update"), ("resync", "--full")] {
		let output = run(&[gone]);
		assert!(!output.status.success());
		assert!(String::from_utf8_lossy(&output.stderr).contains(instead), "{gone}");
	}
	assert_eq!(h.get("/status")["projects"][0]["version"], version_before);
	let output = run(&["snapshot", "--nope"]);
	assert!(!output.status.success());
	assert!(String::from_utf8_lossy(&output.stderr).contains("no flag --nope"));
}

// A new copy was built from the files: its first connect rewrites none of them.
fn new_copy_keeps_model_files(h: &mut Harness) {
	let session = "fa57c0b1";
	let file = "src/shared/Tree.rbxm";
	let before = h.read(file);
	let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
	let mut registry = h.registry();
	registry["sessions"][session] =
		json!({ "dir": h.project, "file": h.tmp.join("copy.rbxl"), "createdAt": "2026-01-01T00:00:00.000Z", "builtAt": now + 60_000 });
	h.set_registry(&registry);
	let base = h.base.clone();
	let path = encode(r#"["ReplicatedStorage","shared","Tree"]"#);
	let send = |h: &Harness, text: &str| -> Option<Value> {
		let route = format!(
			"{base}/save?placeId=0&session={session}&client=fresh&plugin=test&upload=c{}&mode=connect&base=&path={path}&last=1",
			text.len()
		);
		let (status, body) = h.raw_post(&route, &BASE64.encode(text));
		(status == 200).then(|| serde_json::from_str(&body).unwrap())
	};
	let r = until(|| send(h, "as Studio serializes it"));
	assert_eq!(r["action"], "none", "{r}");
	assert_eq!(h.read(file), before, "the file stays");
	assert_eq!(send(h, "as Studio serializes it").unwrap()["action"], "none", "agreed now");

	// a file newer than the build loads in the copy
	let session2 = "fa57c0b2";
	let mut registry = h.registry();
	registry["sessions"][session2] =
		json!({ "dir": h.project, "file": h.tmp.join("copy2.rbxl"), "createdAt": "2026-01-01T00:00:00.000Z", "builtAt": 1 });
	h.set_registry(&registry);
	let route =
		format!("{base}/save?placeId=0&session={session2}&client=fresh2&plugin=test&upload=n&mode=connect&base=&path={path}&last=1");
	let r: Value = until(|| {
		let (status, body) = h.raw_post(&route, &BASE64.encode("older"));
		(status == 200).then(|| serde_json::from_str(&body).unwrap())
	});
	assert_eq!(r["action"], "load", "{r}");
	assert_eq!(h.read(file), before, "the file stays");
}

fn commands_go_where_from_says(h: &mut Harness) {
	let session = "c0ffee00";
	let mut registry = h.registry();
	registry["sessions"] =
		json!({ session: { "dir": h.project, "file": h.tmp.join("copy.rbxl"), "createdAt": "2026-01-01T00:00:00.000Z" } });
	h.set_registry(&registry);
	let base = h.base.clone();
	let as_copy = |route: &str| format!("{base}{route}{}placeId=0&client=copy-client&plugin=test&session={session}", sep(route));
	let copy_tree: Value = until(|| {
		let (status, body) = h.raw_get(&as_copy("/tree"));
		(status == 200).then(|| serde_json::from_str(&body).unwrap())
	});
	let query = format!(
		"placeId=0&client=copy-client&plugin=test&session={session}&epoch={}&version={}",
		copy_tree["epoch"].as_str().unwrap(),
		copy_tree["version"]
	);
	let mut copy = Socket::open(&h.base, &query);
	until(|| (h.get("/status")["projects"][0]["clients"].as_array().unwrap().len() == 2).then_some(()));

	let thing = encode(r#"["Workspace","Thing"]"#);
	let dir = h.dir_query();
	let request = |from: &str| format!("{base}/pull/request?{dir}&path={thing}{from}");
	let (status, text) = h.raw_post(&request(""), "");
	assert_eq!(status, 409);
	assert!(text.contains(&format!("copy {session}")) && text.contains(&format!("place {PLACE}")), "{text}");
	let (status, text) = h.raw_post(&request("&from=nope"), "");
	assert_eq!(status, 409);
	assert!(text.contains("no connected Studio is nope"), "{text}");

	let pulled = spawn_post(&h.agent, request(&format!("&from={session}")));
	let command = copy.next(&["pull"]);
	let place_heard = h.studio().next_within(Duration::from_millis(300));
	assert!(place_heard.as_ref().is_none_or(|m| m["type"] == "version"), "the place's Studio is not asked: {place_heard:?}");
	h.raw_post(&as_copy(&format!("/pull?id={}&error={}", command["id"].as_str().unwrap(), encode("answered by the copy"))), "");
	let (_, result) = pulled.join().unwrap();
	assert_eq!(result["error"], "answered by the copy");

	// a Studio that closes is gone at once, and so are the commands it never answered
	let pulled = spawn_post(&h.agent, request(&format!("&from={session}")));
	copy.next(&["pull"]);
	copy.close();
	let (_, result) = pulled.join().unwrap();
	assert_eq!(result["error"], "Studio disconnected before it answered", "{result}");
	let clients = h.get("/status")["projects"][0]["clients"].clone();
	assert_eq!(clients.as_array().unwrap().len(), 1, "{clients}");

	let pulled = spawn_post(&h.agent, request(""));
	let command = h.next_command();
	h.raw_post(
		&format!("{}/pull?placeId={PLACE}&id={}&error={}", h.base, command["id"].as_str().unwrap(), encode("answered by the place")),
		"",
	);
	let (_, result) = pulled.join().unwrap();
	assert_eq!(result["error"], "answered by the place");
}
