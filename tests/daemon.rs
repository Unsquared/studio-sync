// End to end against a real daemon on a spare port, with a throwaway project
// and a fake plugin speaking HTTP and its WebSocket. Needs `rojo` on PATH.

use serde_json::{Value, json};
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
		let address = base.trim_start_matches("http://");
		let stream = TcpStream::connect(address).unwrap();
		let (socket, _) = tokio_tungstenite::tungstenite::client(format!("ws://{address}/socket?{query}"), stream).unwrap();
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
	model_edits_save_and_conflict(&mut h);
	snapshot_replaces_dir(&mut h);
	build_layers_service_settings(&h);
	commands_go_where_from_says(&mut h);
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
	assert!(h.node("ReplicatedStorage.shared.data")["source"].as_str().unwrap().contains("x = {\n\t\t1,"));
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

// A model from Studio is saved over its file; a file it did not come from is backed up first.
fn model_edits_save_and_conflict(h: &mut Harness) {
	let tree = until(|| {
		let node =
			h.get("/tree")["nodes"].as_array().unwrap().iter().find(|n| joined(&n["path"]) == "ReplicatedStorage.shared.Tree").cloned();
		node.filter(|n| n["kind"] == "model")
	});
	let base = tree["modelHash"].as_str().unwrap().to_string();
	let path = encode(&tree["path"].to_string());
	let save = |base: &str, upload: &str, data: &str| -> Value {
		let route = format!("{}/save?placeId={PLACE}&upload={upload}&base={base}&path={path}", h.base);
		h.raw_post(&format!("{route}&part=1&last=0"), &data[..4]);
		serde_json::from_str(&h.raw_post(&format!("{route}&part=2&last=1"), &data[4..]).1).unwrap()
	};
	let saved = save(&base, "m1", "ZWRpdGVkIGluIHN0dWRpbw=="); // "edited in studio"
	assert_eq!(saved["ok"], true, "{saved}");
	assert_eq!(h.read("src/shared/Tree.rbxm"), "edited in studio");
	assert_ne!(saved["modelHash"], base);

	assert!(saved["replaced"].is_null(), "the file was what Studio loaded: {saved}");

	let other = save(&base, "m2", "c3R1ZGlvIHdpbnM="); // "studio wins", from a file older than the one on disk
	assert_eq!(other["ok"], true, "{other}");
	assert_eq!(h.read("src/shared/Tree.rbxm"), "studio wins");
	let replaced = Path::new(other["replaced"].as_str().unwrap()).join("src/shared/Tree.rbxm");
	assert_eq!(fs::read_to_string(replaced).unwrap(), "edited in studio");

	let script = encode(r#"["ReplicatedStorage","shared","a"]"#);
	let refused: Value =
		serde_json::from_str(&h.raw_post(&format!("{}/save?placeId={PLACE}&upload=m3&base=x&path={script}&last=1", h.base), "eA==").1)
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
	fs::remove_file(h.project.join("src/shared/Tree.rbxm")).unwrap(); // the pull test's fake bytes are no model Rojo can read
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
