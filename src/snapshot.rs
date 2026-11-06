// Snapshots and builds: the open Studio's whole place, minus what the project's
// files own, saved into the repo (`snapshot.dir`, default "place/"), and a place
// file built from the project plus that snapshot, so what ships is one commit.
//
// place/snapshot.json   what was saved: every instance's path and file, each
//                       service's saved properties and attributes (the
//                       explicit value form), what couldn't be saved
// place/<Service>/...   one .rbxm per top-level instance

use crate::config::Config;
use crate::log;
use crate::paths::staging_root;
use crate::tree::key_of;
use crate::util::{Error, Res, copy, mkdirs, read_json, remove, sha, slashes, write};
use crate::writeback::backup_dir;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// The project file's tree as the plugin needs it: which instances the files own.
pub fn plan(dir: &Path, config: &Config) -> Res<Value> {
	fn to_node(body: &Value) -> Value {
		let mut children = Map::new();
		for (name, child) in body.as_object().into_iter().flatten() {
			if !name.starts_with('$') && child.is_object() {
				children.insert(name.clone(), to_node(child));
			}
		}
		json!({ "owned": body["$path"].is_string(), "children": children })
	}
	let project = read_json(&dir.join(&config.project))?;
	let ignore = config.snapshot.as_ref().map(|s| s.ignore.clone()).unwrap_or_default();
	Ok(json!({ "tree": to_node(&project["tree"])["children"], "ignore": ignore }))
}

fn safe_segment(name: &str) -> String {
	let cleaned: String = name
		.chars()
		.map(|c| if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c < '\x20' { '_' } else { c })
		.collect();
	let kept = cleaned.trim_end_matches([' ', '.']);
	match (kept.len() < cleaned.len(), kept.is_empty()) {
		(true, _) => format!("{kept}_"),
		(false, true) => "_".into(),
		(false, false) => cleaned,
	}
}

// One snapshot on its way in: the staging dir, and each instance's file.
pub struct Upload {
	dir: PathBuf,
	files: HashMap<String, PathBuf>,
	parts: HashMap<String, String>,
}

pub type Uploads = HashMap<String, Upload>;

fn upload_for<'a>(uploads: &'a mut Uploads, project_dir: &Path, token: &str) -> Res<&'a mut Upload> {
	if !uploads.contains_key(token) {
		let dir = staging_root().join(format!("snapshot-{}", &sha(format!("{}{token}", project_dir.display()))[..10]));
		remove(&dir)?;
		mkdirs(&dir)?;
		uploads.insert(token.to_string(), Upload { dir, files: HashMap::new(), parts: HashMap::new() });
	}
	Ok(uploads.get_mut(token).expect("just inserted"))
}

// One part of one instance's .rbxm, base64.
pub fn receive_file(uploads: &mut Uploads, project_dir: &Path, token: &str, path: &[String], body: &str, last: bool) -> Res<()> {
	let upload = upload_for(uploads, project_dir, token)?;
	let key = key_of(path);
	upload.parts.entry(key.clone()).or_default().push_str(body);
	if !last {
		return Ok(());
	}
	let data = upload.parts.remove(&key).unwrap_or_default();
	// file systems that ignore case would merge "Map" and "map"
	let taken: HashSet<String> = upload.files.values().map(|f| f.to_string_lossy().to_lowercase()).collect();
	let base: PathBuf = path.iter().map(|n| safe_segment(n)).collect();
	let with = |suffix: &str| {
		let mut file = base.clone().into_os_string();
		file.push(suffix);
		PathBuf::from(file)
	};
	let mut rel = with(".rbxm");
	let mut n = 2;
	while taken.contains(&rel.to_string_lossy().to_lowercase()) {
		rel = with(&format!("~{n}.rbxm"));
		n += 1;
	}
	let full = upload.dir.join(&rel);
	mkdirs(full.parent().unwrap_or(&upload.dir))?;
	write(&full, BASE64.decode(data.trim()).map_err(|e| Error(e.to_string()))?)?;
	upload.files.insert(key, rel);
	Ok(())
}

#[derive(Deserialize)]
pub struct Report {
	#[serde(rename = "placeId", default)]
	place_id: Value,
	entries: Vec<ReportEntry>,
	#[serde(default)]
	services: Map<String, Value>,
	#[serde(default)]
	skipped: Vec<String>,
	#[serde(default)]
	duplicates: Vec<String>,
}

#[derive(Deserialize)]
struct ReportEntry {
	path: Vec<String>,
	#[serde(rename = "className")]
	class_name: String,
}

// Replaces the snapshot dir with the upload (the old one goes to backups); returns the result for the CLI.
pub fn commit(uploads: &mut Uploads, project_dir: &Path, name: &str, config: &Config, token: &str, report: Report) -> Res<Value> {
	upload_for(uploads, project_dir, token)?;
	let upload = uploads.remove(token).expect("just made");
	let mut entries = Vec::new();
	for entry in &report.entries {
		let Some(file) = upload.files.get(&key_of(&entry.path)) else {
			crate::bail!("{} was never uploaded", entry.path.join("."));
		};
		entries.push(json!({ "path": entry.path, "className": entry.class_name, "file": slashes(file) }));
	}
	let manifest = json!({
		"placeId": report.place_id,
		"entries": entries,
		"services": report.services,
		"skipped": report.skipped,
		"duplicates": report.duplicates,
	});
	write(&upload.dir.join("snapshot.json"), format!("{}\n", tabbed(&manifest)?))?;
	let snapshot_dir = config.snapshot_dir();
	let target = project_dir.join(snapshot_dir);
	if target.exists() {
		let backup = backup_dir(name, "snapshot");
		mkdirs(backup.parent().unwrap_or(&backup))?;
		copy(&target, &backup)?;
		remove(&target)?;
	}
	mkdirs(target.parent().unwrap_or(project_dir))?;
	copy(&upload.dir, &target)?;
	remove(&upload.dir)?;
	log!("[{name}] snapshot: {} instance(s) -> {snapshot_dir}/", report.entries.len());
	Ok(json!({
		"ok": true,
		"dir": snapshot_dir,
		"entries": report.entries.len(),
		"skipped": report.skipped,
		"duplicates": report.duplicates,
		"top": report.entries.iter().map(|e| e.path.join(".")).collect::<Vec<_>>(),
	}))
}

// JSON indented with tabs, like the files the TypeScript version wrote.
pub fn tabbed(value: &Value) -> Res<String> {
	let mut out = Vec::new();
	let mut serializer = serde_json::Serializer::with_formatter(&mut out, serde_json::ser::PrettyFormatter::with_indent(b"\t"));
	serde::Serialize::serialize(value, &mut serializer)?;
	Ok(String::from_utf8(out).expect("JSON is UTF-8"))
}

// -- build --------------------------------------------------------------------

pub struct BuildOptions {
	pub output: PathBuf,
	pub snapshot: bool,
}

fn entry<'a>(node: &'a mut Value, key: &str, default: impl FnOnce() -> Value) -> &'a mut Value {
	let object = node.as_object_mut().expect("project tree nodes are objects");
	object.entry(key).or_insert_with(default)
}

// Merges `extra` under `node[key]`; what `node` already has wins.
fn merge_under(node: &mut Value, key: &str, extra: &Value) {
	let Some(extra) = extra.as_object() else { return };
	let mut merged = extra.clone();
	if let Some(own) = node[key].as_object() {
		merged.extend(own.clone());
	}
	node[key] = Value::Object(merged);
}

// The project tree with the snapshot grafted in. The project's own entries win.
pub fn build_project(dir: &Path, config: &Config, options: &BuildOptions, notes: &mut Vec<String>) -> Res<(Value, PathBuf)> {
	let project_file = dir.join(&config.project);
	let mut project = read_json(&project_file)?;
	let base = project_file.parent().unwrap_or(dir).to_path_buf();
	let snap_dir = dir.join(config.snapshot_dir());
	let manifest_file = snap_dir.join("snapshot.json");
	let tree = entry(&mut project, "tree", || json!({ "$className": "DataModel" }));
	if options.snapshot && manifest_file.exists() {
		let manifest = read_json(&manifest_file)?;
		let rel_dir = relative(&snap_dir, &base);
		for (service, settings) in manifest["services"].as_object().into_iter().flatten() {
			let node = entry(tree, service, || json!({ "$className": settings["className"] }));
			merge_under(node, "$properties", &settings["properties"]);
			merge_under(node, "$attributes", &settings["attributes"]);
		}
		let entries = manifest["entries"].as_array().map(Vec::as_slice).unwrap_or_default();
		for item in entries {
			let path: Vec<String> = serde_json::from_value(item["path"].clone())?;
			let Some((leaf, parents)) = path.split_last() else { continue };
			let mut node = &mut *tree;
			for name in parents {
				node = entry(node, name, || json!({}));
			}
			if node.get(leaf).is_some() {
				notes.push(format!("{}: the project file declares it; the snapshot's copy is left out", path.join(".")));
				continue;
			}
			let file = item["file"].as_str().unwrap_or_default();
			node[leaf.as_str()] = json!({ "$path": slashes(&rel_dir.join(file)) });
		}
		notes.push(format!("snapshot: {} instance(s)", entries.len()));
	} else if options.snapshot {
		notes.push(format!("no snapshot in {}/ (studio-sync snapshot): code only", config.snapshot_dir()));
	}
	Ok((project, base))
}

// `path` relative to `from`, both absolute (path.relative).
pub fn relative(path: &Path, from: &Path) -> PathBuf {
	let path: Vec<_> = path.components().collect();
	let from: Vec<_> = from.components().collect();
	let common = path.iter().zip(&from).take_while(|(a, b)| a == b).count();
	let mut out: PathBuf = from[common..].iter().map(|_| "..").collect();
	out.extend(&path[common..]);
	out
}

pub fn run(command: &str, args: &[&str], cwd: &Path) -> Res<String> {
	let output = Command::new(command)
		.args(args)
		.current_dir(cwd)
		.stdin(Stdio::null())
		.output()
		.map_err(|e| format!("{command}: {e} (is it installed? rokit install)"))?;
	let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr);
		let message = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
		crate::bail!("{command} {}: {message}", args.first().unwrap_or(&""));
	}
	Ok(stdout)
}

// The project with the snapshot grafted in. Service settings a plugin
// can't read (Workspace.SignalBehavior, Lighting.Technology, ...) come from the
// project file's $properties, which win over the snapshot's.
pub fn build(dir: &Path, config: &Config, options: &BuildOptions) -> Res<Vec<String>> {
	let mut notes = Vec::new();
	let (project, _) = build_project(dir, config, options, &mut notes)?;
	let output = dir.join(&options.output);
	mkdirs(output.parent().unwrap_or(dir))?;
	crate::build::place(dir, &config.project, &project, &output, &mut notes)?;
	notes.push(format!("built {}", options.output.display()));
	Ok(notes)
}
