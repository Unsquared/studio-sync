// The `studio-sync` command.

mod build;
mod changes;
mod client;
mod config;
mod daemon;
mod events;
mod install;
mod layout;
mod log;
mod modeldiff;
mod open;
mod paths;
mod project;
mod registry;
mod snapshot;
mod tree;
mod util;
mod watch;
mod writeback;

use config::{CONFIG_FILE, Config, LOCAL_CONFIG_FILE};
use paths::{PORT, VERSION, events_file, real};
use registry::Registry;
use serde_json::{Value, json};
use snapshot::BuildOptions;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::LazyLock;
use std::time::Instant;
use util::{Res, read_json, read_text, write};

const USAGE: &str = "  wait                 apply file edits to Studio and report what it holds
  status               connected Studios, copies and errors
  changes [file]       what happened that git doesn't show
  open [--copy]        open the dev place, or a private copy
  close                close this checkout's copies
  pull <path> [file]   save a Studio instance as .rbxm
  snapshot             save the whole place to place/
  build [-o file]      build a place file

  add                  set up a project here
  install              set up this machine, or update it

Details: studio-sync <command> --help
";

// The details of a command.
fn help(command: &str) -> Option<&'static str> {
	Some(match command {
		"wait" => {
			"wait [--timeout s] [--full]
  Waits until Studio's model edits are saved and Studio applies every file change, then checks what Studio holds.
  Exit 0: in sync. 2: no Studio. 3: Studio reported errors, lacks something, or has other script text. 4: timeout.
  --full: a full sync first, for when Studio lost something and `wait` still says in sync.
"
		}
		"status" => {
			"status
  The daemon, this project (outside a project: every project), its Studios and copies, and the errors.
"
		}
		"changes" => {
			"changes [file] [--since 1h] [--rev HEAD] [-n 10] [--raw]
  What happened here that your file edits and git don't say: models saved from Studio, things gone from Studio, backups.
  With an .rbxm: also what differs from git's version of it (--rev: another version). With other text: only what names it.
  -n: more lines, and full instance paths.
  --raw: the last events of every project as JSON lines, to debug the tool (-n, default 40). It is long: pipe it through grep.
"
		}
		"open" => {
			"open [placeId] [--force]
  Opens the dev place (or placeId) in Studio, makes this checkout the one that syncs it, and waits for the sync.
  If a Studio on it syncs with another checkout, it refuses. --force switches that Studio to this checkout's code.
open --copy [--code-only]
  Builds a private local copy of the place from this checkout and opens it. It syncs with this checkout only.
  --code-only: faster, without the snapshot.
"
		}
		"close" => {
			"close [session]
  Closes the copies of this checkout (or one copy) without a save, and deletes their files.
"
		}
		"pull" => {
			"pull <Instance.Path> [file] [--from <copy session | place id>]
  Saves a Studio instance into the repo as .rbxm (models, parts, maps).
  With several Studios connected to the project, --from names the one.
"
		}
		"snapshot" => {
			"snapshot [--from <copy session | place id>]
  Saves the whole open place, except the project's code, into the repo (snapshot.dir, default place/).
  With several Studios connected to the project, --from names the one.
"
		}
		"build" => {
			"build [-o file]
  Builds a place file from the project and the snapshot (default build/place.rbxl).
"
		}
		"add" => {
			"add [dir]
  Registers a project, and writes studio-sync.json if it has none. Deleting that file or the folder unregisters it.
"
		}
		"install" => {
			"install [--update [--force]] [--remove]
  Installs the plugin, the background service and the Claude skill, and restarts the daemon.
  --update: gets the new version first. It refuses while a Studio is connected; --force goes ahead, and each does a full sync.
  --remove: removes them.
"
		}
		_ => return None,
	})
}

// What became of a command that is gone.
fn replaced(command: &str) -> Option<&'static str> {
	Some(match command {
		"remove" => "delete the project's studio-sync.json, or its folder: it unregisters by itself",
		"claim" => "`studio-sync open` in the checkout makes it the one that syncs",
		"list" | "copies" => "`studio-sync status`",
		"log" => "`studio-sync changes --raw`",
		"restart" | "plugin" => "`studio-sync install`",
		"update" => "`studio-sync install --update`",
		"uninstall" => "`studio-sync install --remove`",
		"resync" => "`studio-sync wait --full`",
		_ => return None,
	})
}

static STARTED: LazyLock<Instant> = LazyLock::new(Instant::now);

// Into events.jsonl: this command, and how it ended.
fn record_command(code: i32, message: Option<String>) {
	let args: Vec<String> = std::env::args().skip(1).collect();
	if args.first().is_none_or(|command| command != "serve") {
		let cwd = std::env::current_dir().unwrap_or_default();
		let ms = STARTED.elapsed().as_millis() as u64;
		events::record("cli", json!({ "args": args, "cwd": cwd, "ms": ms, "code": code, "message": message, "version": VERSION }));
	}
}

fn fail(message: impl std::fmt::Display, code: i32) -> ! {
	let message = message.to_string();
	eprintln!("{message}");
	record_command(code, Some(message));
	exit(code)
}

trait OrFail<T> {
	fn or_fail(self) -> T;
}

impl<T> OrFail<T> for Res<T> {
	fn or_fail(self) -> T {
		self.unwrap_or_else(|e| fail(e, 1))
	}
}

fn print(lines: Vec<String>) {
	for line in lines {
		println!("{line}");
	}
}

fn cwd() -> PathBuf {
	std::env::current_dir().unwrap_or_else(|e| fail(format!("the current directory: {e}"), 1))
}

// The first argument that is not a flag, as a real path; else the current directory.
fn target(args: &[String]) -> PathBuf {
	let dir = args.iter().find(|a| !a.starts_with('-')).map(PathBuf::from).unwrap_or_else(cwd);
	real(&dir).unwrap_or_else(|e| fail(format!("{}: {e}", dir.display()), 1))
}

// The project dir containing the current directory, and its merged config.
fn project_here() -> (PathBuf, Config) {
	let start = real(&cwd()).unwrap_or_else(|_| cwd());
	let Some(dir) = start.ancestors().find(|d| d.join(CONFIG_FILE).exists()) else {
		fail(format!("no {CONFIG_FILE} in {} or above (studio-sync add)", start.display()), 1)
	};
	(dir.to_path_buf(), Config::load(dir).or_fail())
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
	let at = args.iter().position(|a| a == name)?;
	args.get(at + 1).map(String::as_str)
}

// The Studio a pull or snapshot goes to (`--from`), and the arguments without it.
fn from_flag(args: &[String]) -> (Option<String>, Vec<String>) {
	let Some(at) = args.iter().position(|a| a == "--from") else {
		return (None, args.to_vec());
	};
	match args.get(at + 1) {
		Some(value) if !value.starts_with('-') => {
			let rest = args.iter().enumerate().filter(|(i, _)| *i != at && *i != at + 1).map(|(_, a)| a.clone()).collect();
			(Some(value.clone()), rest)
		}
		_ => fail("--from needs a copy session or a place id (studio-sync status lists them)", 1),
	}
}

// "ReplicatedStorage.shared.Thing" or "game.Workspace.Map"; names with dots go in brackets: Workspace["a.b"]
fn parse_instance_path(text: &str) -> Vec<String> {
	let mut parts = Vec::new();
	let mut rest = text;
	while !rest.is_empty() {
		let quoted = rest.strip_prefix('[').map(str::trim_start).and_then(|r| r.strip_prefix('"'));
		if let Some((name, after)) = quoted.and_then(|q| q.split_once('"'))
			&& let Some(after) = after.trim_start().strip_prefix(']')
		{
			parts.push(name.to_string());
			rest = after;
			continue;
		}
		let end = rest.find(['.', '[', ']']).unwrap_or(rest.len());
		if end == 0 {
			rest = &rest[1..];
		} else {
			parts.push(rest[..end].to_string());
			rest = &rest[end..];
		}
	}
	if parts.first().is_some_and(|p| p == "game") {
		parts.remove(0);
	}
	if parts.is_empty() {
		fail(format!("no instance path in \"{text}\""), 1);
	}
	parts
}

// The daemon's JSON answer to a command, or its error text.
fn command_result(response: Res<(u16, String)>) -> Value {
	let (_, text) = response.or_fail();
	serde_json::from_str(&text).unwrap_or_else(|_| fail(&text, 1))
}

fn strings(value: &Value) -> Vec<&str> {
	value.as_array().into_iter().flatten().filter_map(Value::as_str).collect()
}

fn add(args: &[String]) {
	let dir = target(args);
	let config = dir.join(CONFIG_FILE);
	if !config.exists() {
		let project_file = dir.join("default.project.json");
		let roots = match read_json(&project_file) {
			Ok(project) => tree::default_roots(&project).0,
			Err(_) => Vec::new(),
		};
		let text = snapshot::tabbed(&json!({ "placeIds": [], "project": "default.project.json", "roots": roots })).or_fail();
		write(&config, format!("{text}\n")).or_fail();
		println!("wrote {}: put your dev place id in placeIds (or in {LOCAL_CONFIG_FILE}, untracked)", config.display());
	}
	let mut registry = Registry::read().or_fail();
	registry.add(&dir);
	registry.write().or_fail();
	println!("added {}", dir.display());
}

// Makes `dir` the checkout that syncs its places. False: nothing else had them. True: a Studio on them
// synced with another checkout and now moves here, so it needs no launch.
fn take_places(dir: &Path, ids: &[u64], force: bool) -> bool {
	let status = client::status().or_fail();
	let other = status["projects"].as_array().into_iter().flatten().find(|p| {
		p["dir"].as_str() != dir.to_str()
			&& p["clients"].as_array().is_some_and(|clients| {
				clients.iter().any(|c| c["session"].is_null() && c["placeId"].as_u64().is_some_and(|id| ids.contains(&id)))
			})
	});
	if let (Some(other), false) = (other, force) {
		fail(
			format!(
				"a Studio on this place syncs with {} now, and opening it here gives it this checkout's code. To test this checkout, use `studio-sync open --copy`. Someone else may be using that Studio; to switch it anyway: --force",
				other["dir"].as_str().unwrap_or_default()
			),
			1,
		);
	}
	let mut registry = Registry::read().or_fail();
	if ids.iter().all(|id| registry.claims.get(&id.to_string()).is_some_and(|owner| owner == dir)) {
		return false;
	}
	registry.add(dir);
	for id in ids {
		registry.claims.insert(id.to_string(), dir.to_path_buf());
	}
	registry.write().or_fail();
	other.is_some()
}

fn status() {
	let status = client::status().or_fail();
	let version = status["version"].as_str().unwrap_or("?");
	println!("daemon {version} pid {} on port {}", status["pid"], *PORT);
	if version != VERSION {
		println!("  the daemon is {version}, this checkout is {VERSION}: studio-sync install");
	}
	let projects = status["projects"].as_array().map(Vec::as_slice).unwrap_or_default();
	let here = real(&cwd()).unwrap_or_else(|_| cwd());
	let mine = here.ancestors().find_map(|d| projects.iter().find(|p| p["dir"].as_str() == d.to_str()));
	let shown: Vec<&Value> = match mine {
		Some(project) => vec![project],
		None => projects.iter().collect(),
	};
	let sessions = Registry::read().map(|registry| registry.sessions).unwrap_or_default();
	for p in &shown {
		let claimed = if p["claimed"].as_array().is_some_and(|c| !c.is_empty()) { " (claimed)" } else { "" };
		let places: Vec<String> = p["placeIds"].as_array().into_iter().flatten().map(Value::to_string).collect();
		let state = if p["active"] == true {
			format!("v{}  {} instances", p["version"], p["nodes"])
		} else {
			"idle (builds its tree when a Studio connects)".into()
		};
		println!("\n{}  {state}  places {}{claimed}", p["name"].as_str().unwrap_or_default(), places.join(", "));
		println!("  {}", p["dir"].as_str().unwrap_or_default());
		let read_only = strings(&p["readOnly"]);
		let read_only = if read_only.is_empty() { String::new() } else { format!("  read-only: {}", read_only.join(", ")) };
		println!("  roots: {}{read_only}", strings(&p["roots"]).join(", "));
		println!(
			"  writeback: {}",
			if p["writeback"] == true { "on (Studio model edits save to their files)" } else { "off (disk -> Studio)" }
		);
		if let Some(error) = p["error"].as_str() {
			println!("  ERROR {error}");
		}
		if !Path::new(p["dir"].as_str().unwrap_or_default()).join(CONFIG_FILE).exists() {
			println!("  no {CONFIG_FILE} there any more, so it is unregistered now. To bring it back: studio-sync add");
		}
		for play in p["plays"].as_array().into_iter().flatten() {
			let state = if play["stale"] == true {
				format!("runs v{}, older: restart it", play["version"])
			} else {
				format!("runs v{}", play["version"])
			};
			println!("  Play: {}, {state}", play["who"].as_str().unwrap_or_default());
		}
		let clients = p["clients"].as_array().map(Vec::as_slice).unwrap_or_default();
		let here = p["dir"].as_str().map(Path::new);
		for (id, session) in &sessions {
			if Some(session.dir.as_path()) == here && !clients.iter().any(|c| c["session"].as_str() == Some(id)) {
				println!("  copy {id}: no Studio connected (studio-sync close {id})");
			}
		}
		if p["connecting"] == true {
			println!("  Studio: connecting (a full sync is running)");
		} else if clients.is_empty() {
			println!("  Studio: not connected");
		}
		for c in clients {
			let lag = if c["version"] == p["version"] { "in sync".into() } else { format!("at v{}, behind", c["version"]) };
			let who = match c["session"].as_str() {
				Some(session) => format!("copy {session}"),
				None => format!("place {}", c["placeId"]),
			};
			let plugin = c["plugin"].as_str().unwrap_or("?");
			println!("  Studio: {who}, {lag}, plugin {plugin}");
			if plugin != VERSION {
				println!("    plugin {plugin} is not {VERSION}: run `studio-sync install` and restart Studio");
			}
			for error in strings(&c["errors"]) {
				println!("    error: {error}");
			}
			for name in strings(&c["missing"]) {
				println!("    gone from Studio: {name}");
			}
			for name in strings(&c["edited"]) {
				println!("    differs from its file: {name}");
			}
		}
	}
	let others = projects.len() - shown.len();
	if others > 0 {
		println!("\n{others} other project(s): run status outside a project to see them");
	}
	// a project without its config has nothing to sync: writing the registry drops it
	if let Ok(registry) = Registry::read()
		&& registry.projects.iter().any(|dir| !dir.join(CONFIG_FILE).exists())
	{
		registry.write().or_fail();
	}
}

// `wait` runs after every batch of edits: a long list goes to `status` or `changes`.
const WAIT_NAMES: usize = 10;

fn wait(args: &[String]) {
	if args.iter().any(|a| a == "--full") {
		let dir = cwd();
		let result = command_result(client::post(&format!("/resync?{}", client::query(&[("dir", &dir.to_string_lossy())]))));
		if result["ok"] != true {
			fail(&result, 1);
		}
	}
	let timeout: u64 = flag(args, "--timeout").and_then(|t| t.parse().ok()).unwrap_or(30);
	let dir = cwd();
	let (code, text) =
		client::get(&format!("/wait?{}", client::query(&[("dir", &dir.to_string_lossy()), ("timeout", &timeout.to_string())]))).or_fail();
	if code != 200 {
		fail(text, 1);
	}
	let result: Value = serde_json::from_str(&text).unwrap_or_else(|_| fail(&text, 1));
	let project = result["project"].as_str().unwrap_or_default();
	if let Some(error) = result["error"].as_str() {
		fail(format!("{project}: the last sync failed: {error}"), 3);
	}
	if result["ok"] != true {
		if result["reason"] == "no-studio" {
			fail(format!("{project} v{}: no Studio is connected (open the dev place in Edit; `studio-sync status`)", result["version"]), 2);
		}
		let behind: Vec<String> = result["clients"]
			.as_array()
			.into_iter()
			.flatten()
			.filter(|c| c["version"] != result["version"])
			.map(|c| format!("v{}", c["version"]))
			.collect();
		fail(
			format!(
				"{project}: Studio has not applied v{} after {timeout}s (it is at {}). If it stays there: studio-sync wait --full",
				result["version"],
				behind.join(", ")
			),
			4,
		);
	}
	let saved = strings(&result["saved"]);
	for file in &saved[..saved.len().min(WAIT_NAMES)] {
		println!("saved from Studio: {file}");
	}
	if saved.len() > WAIT_NAMES {
		println!("+{} more saved from Studio: studio-sync changes", saved.len() - WAIT_NAMES);
	}
	let of_clients =
		|field: &str| -> Vec<&str> { result["clients"].as_array().into_iter().flatten().flat_map(|c| strings(&c[field])).collect() };
	let errors = of_clients("errors");
	if !errors.is_empty() {
		fail(format!("{project} v{} applied with errors:\n  {}", result["version"], listed(&errors, WAIT_NAMES)), 3);
	}
	let missing = of_clients("missing");
	if !missing.is_empty() {
		fail(format!("{project} v{}: gone from Studio, even after a full sync:\n  {}", result["version"], listed(&missing, WAIT_NAMES)), 3);
	}
	let edited = of_clients("edited");
	if !edited.is_empty() {
		fail(
			format!(
				"{project} v{}: Studio's text differs from the file, so tests would not run what is on disk:\n  {}\nPut wanted changes in the files, then: studio-sync wait --full (Studio's text goes to backups)",
				result["version"],
				listed(&edited, WAIT_NAMES)
			),
			3,
		);
	}
	let plays = result["plays"].as_array().map(Vec::as_slice).unwrap_or_default();
	let play = if plays.is_empty() { " (a running Play keeps the code it started with)" } else { "" };
	println!("{project} v{}: Studio is in sync{play}", result["version"]);
	for p in plays {
		let who = p["who"].as_str().unwrap_or_default();
		if p["stale"] == true {
			println!("Play ({who}) started at v{} and runs that code: restart Play to test this version", p["version"]);
		} else {
			println!("Play ({who}) runs this version");
		}
	}
}

fn pull(args: &[String]) {
	let (from, rest) = from_flag(args);
	let Some(instance) = rest.first() else { fail("usage: studio-sync pull <Instance.Path> [file] [--from <copy session | place id>]", 1) };
	let file = rest.get(1).map(|f| {
		let full = std::path::absolute(f).unwrap_or_else(|_| PathBuf::from(f));
		snapshot::relative(&full, &target(&[])).to_string_lossy().into_owned()
	});
	let dir = cwd();
	let path = serde_json::to_string(&parse_instance_path(instance)).unwrap_or_default();
	let mut query = vec![("dir", dir.to_string_lossy().into_owned()), ("path", path), ("file", file.unwrap_or_default())];
	if let Some(from) = from {
		query.push(("from", from));
	}
	let pairs: Vec<(&str, &str)> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
	let result = command_result(client::post(&format!("/pull/request?{}", client::query(&pairs))));
	if result["ok"] != true {
		fail(result["error"].as_str().unwrap_or("pull failed"), 1);
	}
	println!("saved {instance} to {}", result["file"].as_str().unwrap_or_default());
}

fn snapshot_command(args: &[String]) {
	let dir = cwd();
	let dir = dir.to_string_lossy();
	let (from, _) = from_flag(args);
	let mut pairs = vec![("dir", &*dir)];
	if let Some(from) = &from {
		pairs.push(("from", from));
	}
	let result = command_result(client::post(&format!("/snapshot/request?{}", client::query(&pairs))));
	if result["ok"] != true {
		fail(format!("snapshot failed: {}", result["error"].as_str().unwrap_or("?")), 1);
	}
	println!("saved {} instance(s) to {}/:", result["entries"], result["dir"].as_str().unwrap_or_default());
	for line in strings(&result["top"]) {
		println!("  {line}");
	}
	let duplicates = strings(&result["duplicates"]);
	if !duplicates.is_empty() {
		println!("not saved, a sibling has the same name (rename them): {}", duplicates.join(", "));
	}
	let skipped = strings(&result["skipped"]);
	if !skipped.is_empty() {
		println!("not saved: {}", skipped.join(", "));
	}
	println!("review with git status / git diff, then commit; `studio-sync build` makes the place file");
}

fn build(args: &[String]) {
	let (dir, config) = project_here();
	let output = PathBuf::from(flag(args, "-o").unwrap_or("build/place.rbxl"));
	print(snapshot::build(&dir, &config, &BuildOptions { output, snapshot: true }).or_fail());
}

fn open_command(args: &[String]) {
	let (dir, config) = project_here();
	client::status().or_fail(); // fails clearly when the daemon is down
	let lines = if args.iter().any(|a| a == "--copy") {
		open::open_copy(&dir, &config, args.iter().any(|a| a == "--code-only"))
	} else {
		let place_id = args
			.iter()
			.find_map(|a| a.parse::<u64>().ok())
			.or_else(|| config.place_ids.first().copied())
			.unwrap_or_else(|| fail("no place id: pass one, or list it in studio-sync.json placeIds", 1));
		let moved = config.place_ids.contains(&place_id) && take_places(&dir, &config.place_ids, args.iter().any(|a| a == "--force"));
		open::open_place(&dir, place_id, moved)
	};
	print(lines.or_fail());
}

fn changes_command(args: &[String]) {
	if args.iter().any(|a| a == "--raw") {
		let n: usize = flag(args, "-n").and_then(|n| n.parse().ok()).unwrap_or(40);
		let text = read_text(&events_file()).unwrap_or_default();
		let lines: Vec<&str> = text.trim_end().lines().collect();
		return println!("{}", lines[lines.len().saturating_sub(n)..].join("\n"));
	}
	let (dir, _) = project_here();
	let since = flag(args, "--since").unwrap_or("1h");
	let Some(age) = changes::parse_age(since).filter(|age| *age >= 0) else {
		fail(format!("--since {since}: use minutes, hours or days, as 10m, 2h or 1d"), 1)
	};
	let limit: Option<usize> = flag(args, "-n").and_then(|n| n.parse().ok());
	// the first argument that is neither a flag nor a flag's value
	let mut rest = args.iter();
	let mut named = None;
	while let Some(arg) = rest.next() {
		if arg.starts_with('-') {
			rest.next();
		} else {
			named = Some(arg.as_str());
			break;
		}
	}
	let mut shown = false;
	let mut text = named.map(str::to_string);
	if let Some(name) = named {
		let file = cwd().join(name);
		if name.ends_with(".rbxm")
			&& let Some(lines) = modeldiff::of_file(&file, flag(args, "--rev"), limit).or_fail()
		{
			print(lines);
			shown = true;
		}
		if let Ok(full) = real(&file) {
			text = Some(snapshot::relative(&full, &dir).to_string_lossy().replace('\\', "/"));
		}
	}
	let now = chrono::Local::now();
	let since_ms = now.timestamp_millis() - age;
	let project = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
	let filter = changes::Filter { project: &project, since_ms, file: text.as_deref(), lines: limit.unwrap_or(10) };
	let found = changes::summarize(&changes::events_text(since_ms), &filter, now);
	if found.is_empty() && !shown {
		println!("nothing in the last {since}");
	}
	print(found);
}

// At most `max` of them, one on a line, and how many more `status` has.
fn listed(items: &[&str], max: usize) -> String {
	let more = items.len().saturating_sub(max);
	let shown = items[..items.len() - more].join("\n  ");
	if more > 0 { format!("{shown}\n  +{more} more: studio-sync status") } else { shown }
}

// A second daemon (tests, plugin work) has its own home and port, but its binary would still
// replace the real service, plugin and command.
fn real_machine_only() {
	if std::env::var_os("STUDIO_SYNC_HOME").is_some() {
		fail("STUDIO_SYNC_HOME is set: this changes the real service, plugin and command, so it won't run", 1);
	}
}

fn update(args: &[String]) {
	if !args.iter().any(|a| a == "--force")
		&& let Ok(status) = client::status()
	{
		let busy: Vec<&str> = status["projects"]
			.as_array()
			.into_iter()
			.flatten()
			.filter(|p| p["clients"].as_array().is_some_and(|c| !c.is_empty()))
			.filter_map(|p| p["name"].as_str())
			.collect();
		if !busy.is_empty() {
			fail(
				format!(
					"Studios are connected to {}, and someone may be working in them: an update restarts the daemon and each does a full sync. When that is fine: studio-sync install --update --force",
					busy.join(", ")
				),
				1,
			);
		}
	}
	real_machine_only();
	print(install::update().or_fail());
}

// The flags of a command. Anything else stops it before it does its work: a `snapshot --help` once
// ran a real snapshot.
fn known_flags(command: &str) -> Option<&'static [&'static str]> {
	Some(match command {
		"add" | "close" | "serve" | "status" => &[],
		"install" => &["--update", "--force", "--remove"],
		"wait" => &["--timeout", "--full"],
		"pull" | "snapshot" => &["--from"],
		"build" => &["-o"],
		"open" => &["--copy", "--code-only", "--force"],
		"changes" => &["--since", "--rev", "-n", "--raw"],
		_ => return None,
	})
}

fn main() {
	LazyLock::force(&STARTED);
	let args: Vec<String> = std::env::args().skip(1).collect();
	let Some((name, rest)) = args.split_first() else {
		print!("studio-sync {VERSION}: two-way sync between Roblox Studio and your files\n\n{USAGE}");
		return;
	};
	if let Some(flags) = known_flags(name) {
		if rest.iter().any(|a| a == "--help" || a == "-h") {
			print!("{}", help(name).unwrap_or(USAGE));
			return;
		}
		if let Some(bad) = rest.iter().find(|a| a.starts_with('-') && !flags.contains(&a.as_str())) {
			fail(format!("{name}: no flag {bad}. Its flags: {}", if flags.is_empty() { "none".to_string() } else { flags.join(" ") }), 1);
		}
	}
	match name.as_str() {
		"add" => add(rest),
		"status" => status(),
		"wait" => wait(rest),
		"pull" => pull(rest),
		"snapshot" => snapshot_command(rest),
		"build" => build(rest),
		"open" => open_command(rest),
		"close" => {
			let (dir, _) = project_here();
			print(open::close_copies(&dir, rest.iter().find(|a| !a.starts_with('-')).map(String::as_str)).or_fail());
		}
		"changes" => changes_command(rest),
		"install" if rest.iter().any(|a| a == "--update") => update(rest),
		"install" if rest.iter().any(|a| a == "--remove") => {
			real_machine_only();
			print(install::uninstall().or_fail())
		}
		"install" => {
			real_machine_only();
			print(install::install().or_fail())
		}
		"serve" => daemon::serve().or_fail(),
		"--version" | "-V" => println!("{VERSION}"),
		"help" | "--help" | "-h" => print!("studio-sync {VERSION}: two-way sync between Roblox Studio and your files\n\n{USAGE}"),
		other => {
			if let Some(instead) = replaced(other) {
				fail(format!("`{other}` is gone: {instead}"), 1);
			}
			fail(format!("studio-sync {VERSION}: two-way sync between Roblox Studio and your files\n\n{USAGE}"), 1);
		}
	}
	record_command(0, None);
}
