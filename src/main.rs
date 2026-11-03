// The `studio-sync` command.

mod client;
mod config;
mod daemon;
mod install;
mod layout;
mod log;
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
use paths::{PORT, VERSION, log_file, real};
use registry::Registry;
use serde_json::{Value, json};
use snapshot::BuildOptions;
use std::path::PathBuf;
use std::process::exit;
use std::time::Duration;
use util::{Res, read_json, read_text, write};

const USAGE: &str = "  add [dir]              register a project (writes studio-sync.json if it does not exist)
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
                         into the repo (snapshot.dir, default place/)
                         With several Studios connected to the project, --from names the one.
  build [-o file] [--code-only]
                         build a place file from the project and the snapshot (default build/place.rbxl)
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
";

fn fail(message: impl std::fmt::Display, code: i32) -> ! {
	eprintln!("{message}");
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

fn remove(args: &[String]) {
	let dir = target(args);
	let mut registry = Registry::read().or_fail();
	registry.projects.retain(|p| *p != dir);
	registry.claims.retain(|_, owner| *owner != dir);
	registry.write().or_fail();
	println!("removed {}", dir.display());
}

fn claim(args: &[String]) {
	let dir = target(args);
	let ids = Config::load(&dir).or_fail().place_ids;
	if ids.is_empty() {
		fail("this project lists no placeIds", 1);
	}
	let mut registry = Registry::read().or_fail();
	registry.add(&dir);
	for id in &ids {
		registry.claims.insert(id.to_string(), dir.clone());
	}
	registry.write().or_fail();
	let ids: Vec<String> = ids.iter().map(u64::to_string).collect();
	println!("{} now syncs place(s) {}; Studio resyncs within seconds", dir.display(), ids.join(", "));
}

fn status(args: &[String]) {
	let status = client::status().or_fail();
	if args.iter().any(|a| a == "--json") {
		return println!("{}", serde_json::to_string_pretty(&status).unwrap_or_default());
	}
	let version = status["version"].as_str().unwrap_or("?");
	println!("daemon {version} pid {} on port {}", status["pid"], *PORT);
	if version != VERSION {
		println!("  the daemon is {version}, this checkout is {VERSION}: studio-sync restart");
	}
	for p in status["projects"].as_array().into_iter().flatten() {
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
		let clients = p["clients"].as_array().map(Vec::as_slice).unwrap_or_default();
		if clients.is_empty() {
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
				println!("    plugin {plugin} is not {VERSION}: run `studio-sync plugin` and restart Studio");
			}
			for error in strings(&c["errors"]) {
				println!("    error: {error}");
			}
		}
	}
}

fn wait(args: &[String]) {
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
		fail(format!("{project}: Studio has not applied v{} after {timeout}s", result["version"]), 4);
	}
	let errors: Vec<&str> = result["clients"].as_array().into_iter().flatten().flat_map(|c| strings(&c["errors"])).collect();
	if !errors.is_empty() {
		fail(format!("{project} v{} applied with errors:\n  {}", result["version"], errors.join("\n  ")), 3);
	}
	println!("{project} v{}: Studio is in sync", result["version"]);
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
	let snapshot = !args.iter().any(|a| a == "--code-only");
	print(snapshot::build(&dir, &config, &BuildOptions { output, snapshot }).or_fail());
}

fn open_command(args: &[String]) {
	let (dir, config) = project_here();
	let timeout = Duration::from_secs(flag(args, "--timeout").and_then(|t| t.parse().ok()).unwrap_or(180));
	client::status().or_fail(); // fails clearly when the daemon is down
	let lines = if args.iter().any(|a| a == "--copy") {
		open::open_copy(&dir, &config, args.iter().any(|a| a == "--code-only"), timeout)
	} else {
		let place_id = args
			.iter()
			.find_map(|a| a.parse::<u64>().ok())
			.or_else(|| config.place_ids.first().copied())
			.unwrap_or_else(|| fail("no place id: pass one, or list it in studio-sync.json placeIds", 1));
		open::open_place(&dir, place_id, timeout)
	};
	print(lines.or_fail());
}

fn show_log(args: &[String]) {
	let n: usize = flag(args, "-n").and_then(|n| n.parse().ok()).unwrap_or(40);
	let text = read_text(&log_file()).unwrap_or_default();
	let lines: Vec<&str> = text.trim_end().lines().collect();
	println!("{}", lines[lines.len().saturating_sub(n)..].join("\n"));
}

fn restart() {
	let pid = client::status().ok().and_then(|s| s["pid"].as_u64()).map(|p| p as u32);
	println!("{}", install::restart_service(pid));
}

fn main() {
	let args: Vec<String> = std::env::args().skip(1).collect();
	let Some((name, rest)) = args.split_first() else {
		print!("studio-sync {VERSION}: two-way sync between Roblox Studio and your files\n\n{USAGE}");
		return;
	};
	match name.as_str() {
		"add" => add(rest),
		"remove" => remove(rest),
		"claim" => claim(rest),
		"list" => {
			for dir in Registry::read().or_fail().projects {
				println!("{}", dir.display());
			}
		}
		"status" => status(rest),
		"wait" => wait(rest),
		"pull" => pull(rest),
		"snapshot" => snapshot_command(rest),
		"build" => build(rest),
		"open" => open_command(rest),
		"close" => {
			let (dir, _) = project_here();
			print(open::close_copies(&dir, rest.iter().find(|a| !a.starts_with('-')).map(String::as_str)).or_fail());
		}
		"copies" => print(open::copies(rest.iter().any(|a| a == "--clean")).or_fail()),
		"log" => show_log(rest),
		"install" => print(install::install().or_fail()),
		"uninstall" => print(install::uninstall().or_fail()),
		"restart" => restart(),
		"plugin" => {
			let file = install::build_plugin().or_fail();
			println!("plugin written to {}; Studio reloads it (or restart Studio)", file.display());
		}
		"serve" => daemon::serve().or_fail(),
		"--version" | "-V" => println!("{VERSION}"),
		"help" | "--help" | "-h" => print!("studio-sync {VERSION}: two-way sync between Roblox Studio and your files\n\n{USAGE}"),
		_ => {
			eprint!("studio-sync {VERSION}: two-way sync between Roblox Studio and your files\n\n{USAGE}");
			exit(1);
		}
	}
}
