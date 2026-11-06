// The node tree Studio mirrors: the project's layout (layout.rs) and the files it
// names. Node and Kind are the wire format (docs/protocol.md).

use crate::config::Config;
use crate::layout::{self, Entry};
use crate::paths::normalize;
use crate::util::{Res, read, read_json, sha};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const PACKAGE_DIRS: [&str; 3] = ["Packages", "ServerPackages", "DevPackages"];

#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
	Script,    // a .luau file
	Init,      // a directory with an init script: its children live in the directory
	Dir,       // a directory as a Folder (or a class from init.meta.json)
	Generated, // .json / .txt / .toml / .csv turned into an instance: read-only
	Model,     // a .rbxm / .rbxmx file: pushed whole; Studio edits to a .rbxm save back
	Json,      // an instance from a .model.json: read-only
	Project,   // an instance the project file declares
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
	pub path: Vec<String>,
	pub class_name: String,
	pub kind: Kind,
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub ro: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub source: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub source_hash: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub run_context: Option<&'static str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub properties: Option<Map<String, Value>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub attributes: Option<Map<String, Value>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub model: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub model_hash: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
	pub hash: String,
}

// Where a node lives on disk (relative to the project dir); never sent to Studio.
#[derive(Clone, Default)]
pub struct Place {
	pub file: Option<PathBuf>,
	pub dir: Option<PathBuf>,
	pub meta: Option<PathBuf>,
	pub pinned: bool,
	// every file and directory the node is made from
	pub files: Vec<PathBuf>,
}

// Nodes by key. Keys sort parents before their children, so the tree goes out top-down.
pub type Nodes = BTreeMap<String, Arc<Node>>;

// The last tree and the paths changed since (relative to the project dir): a node whose files
// are all unchanged is reused instead of read again.
pub struct Reuse {
	pub nodes: Nodes,
	pub places: BTreeMap<String, Place>,
	pub changed: BTreeSet<PathBuf>,
}

impl Reuse {
	fn node(&self, key: &str, entry: &Entry, files: &[PathBuf]) -> Option<&Arc<Node>> {
		let (node, place) = (self.nodes.get(key)?, self.places.get(key)?);
		let touched = |f: &PathBuf| f.ancestors().any(|a| self.changed.contains(a));
		let model_json = |f: &PathBuf| f.to_string_lossy().ends_with(".model.json");
		(place.files == files && node.class_name == entry.class_name && !files.iter().any(|f| touched(f) || model_json(f))).then_some(node)
	}
}

// The paths an entry is made from, as Place.files keeps them.
fn files_of(entry: &Entry) -> Vec<PathBuf> {
	entry.file_paths.iter().chain(&entry.dir).map(|f| normalize(f)).collect()
}

pub struct Generated {
	pub nodes: Nodes,
	pub places: BTreeMap<String, Place>,
	pub roots: Vec<Vec<String>>,
	pub read_only: Vec<Vec<String>>,
}

pub fn key_of(path: &[String]) -> String {
	path.join("\0")
}

pub fn starts_with(path: &[String], prefix: &[String]) -> bool {
	path.len() >= prefix.len() && path[..prefix.len()] == *prefix
}

pub fn is_package_dir(path: &Path) -> bool {
	path.file_name().and_then(|n| n.to_str()).is_some_and(|n| PACKAGE_DIRS.contains(&n))
}

// Every "$path" in a project tree, by the key of the instance that declares it.
pub fn project_paths(tree: &Value) -> BTreeMap<String, String> {
	fn walk(tree: &Value, here: &mut Vec<String>, out: &mut BTreeMap<String, String>) {
		let Some(entries) = tree.as_object() else { return };
		for (key, value) in entries {
			if key == "$path" {
				if let Some(p) = value.as_str() {
					out.insert(key_of(here), p.to_string());
				}
			} else if !key.starts_with('$') && value.is_object() {
				here.push(key.clone());
				walk(value, here, out);
				here.pop();
			}
		}
	}
	let mut out = BTreeMap::new();
	walk(tree, &mut Vec::new(), &mut out);
	out
}

// Default roots: every service, or service child, the project maps from a $path.
pub fn default_roots(project: &Value) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
	let mut roots = Vec::new();
	let mut read_only = Vec::new();
	let mut add = |path: Vec<String>, fs_path: &str| {
		if is_package_dir(Path::new(fs_path)) {
			read_only.push(path.clone());
		}
		roots.push(path);
	};
	for (service, body) in project["tree"].as_object().into_iter().flatten() {
		if service.starts_with('$') || !body.is_object() {
			continue;
		}
		if let Some(p) = body["$path"].as_str() {
			add(vec![service.clone()], p);
			continue;
		}
		for (name, child) in body.as_object().into_iter().flatten() {
			if let (false, Some(p)) = (name.starts_with('$'), child["$path"].as_str()) {
				add(vec![service.clone(), name.clone()], p);
			}
		}
	}
	(roots, read_only)
}

// JSON as a Luau table literal: the text of the ModuleScript a .json file is. Rojo's text, so a place
// it built holds the same.
pub fn luau_literal(value: &Value) -> String {
	let mut out = String::new();
	write_literal(&mut out, value, 0);
	out
}

fn write_literal(out: &mut String, value: &Value, depth: usize) {
	let indent = |out: &mut String, depth: usize| out.extend(std::iter::repeat_n('\t', depth));
	match value {
		Value::Null => out.push_str("nil"),
		Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
		Value::Number(n) => out.push_str(&n.as_f64().unwrap_or_default().to_string()),
		Value::String(text) => write_string(out, text),
		Value::Array(items) => {
			out.push('{');
			for (index, item) in items.iter().enumerate() {
				if index > 0 {
					out.push_str(", ");
				}
				write_literal(out, item, depth);
			}
			out.push('}');
		}
		Value::Object(fields) => {
			// sorted, so a change of order in the file changes nothing in Studio
			let mut sorted: Vec<_> = fields.iter().collect();
			sorted.sort_by(|a, b| a.0.cmp(b.0));
			out.push_str("{\n");
			for (key, item) in sorted {
				indent(out, depth + 1);
				write_field(out, key);
				out.push_str(" = ");
				write_literal(out, item, depth + 1);
				out.push_str(",\n");
			}
			indent(out, depth);
			out.push('}');
		}
	}
}

fn write_string(out: &mut String, text: &str) {
	out.push('"');
	for c in text.chars() {
		match c {
			'"' => out.push_str("\\\""),
			'\r' => out.push_str("\\r"),
			'\n' => out.push_str("\\n"),
			'\t' => out.push_str("\\t"),
			'\\' => out.push_str("\\\\"),
			_ => out.push(c),
		}
	}
	out.push('"');
}

const KEYWORDS: [&str; 21] = [
	"and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in", "local", "nil", "not", "or", "repeat", "return",
	"then", "true", "until", "while",
];

fn write_field(out: &mut String, key: &str) {
	let mut chars = key.chars();
	let ident = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
	if ident && !KEYWORDS.contains(&key) {
		out.push_str(key);
	} else {
		out.push('[');
		write_string(out, key);
		out.push(']');
	}
}

pub fn generate(dir: &Path, config: &Config, reuse: Option<Reuse>) -> Res<Generated> {
	let project = read_json(&dir.join(&config.project))?;
	let layout = layout::read(dir, &config.project)?;
	let (default_roots, default_read_only) = default_roots(&project);
	let roots = config.roots.clone().unwrap_or(default_roots);
	let read_only = config
		.read_only
		.clone()
		.unwrap_or_else(|| default_read_only.into_iter().filter(|r| roots.iter().any(|root| starts_with(r, root))).collect());
	let root_keys: Vec<String> = roots.iter().map(|r| key_of(r)).collect();
	let mut collector = Collector {
		dir,
		files: read_synced(dir, &layout, &root_keys, reuse.as_ref()),
		reuse,
		root_keys,
		read_only,
		mapped: project_paths(&project["tree"]),
		nodes: Nodes::new(),
		places: BTreeMap::new(),
	};
	collector.collect(&layout, &mut Vec::new(), false, None)?;
	Ok(Generated { nodes: collector.nodes, places: collector.places, roots, read_only: collector.read_only })
}

static NO_MODEL: Value = Value::Null;

// The files of every synced instance that is not reused, read on all cores: opening thousands
// of small files is most of a full regenerate, and the reads are independent.
fn read_synced(dir: &Path, layout: &Entry, root_keys: &[String], reuse: Option<&Reuse>) -> HashMap<PathBuf, Res<Vec<u8>>> {
	let mut files = Vec::new();
	let mut walk = |entry: &Entry, key: &str| {
		if reuse.is_none_or(|r| r.node(key, entry, &files_of(entry)).is_none()) {
			let used = |f: &&PathBuf| !f.to_string_lossy().ends_with(".project.json");
			files.extend(entry.file_paths.iter().filter(used).map(|f| normalize(f)));
		}
	};
	visit_synced(layout, &mut Vec::new(), false, root_keys, &mut walk);
	let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
	std::thread::scope(|scope| {
		let readers: Vec<_> = files
			.chunks(files.len().div_ceil(threads).max(1))
			.map(|part| scope.spawn(move || part.iter().map(|f| (f.clone(), read(&dir.join(f)))).collect::<Vec<_>>()))
			.collect();
		readers.into_iter().flat_map(|r| r.join().expect("a file reader")).collect()
	})
}

fn visit_synced(entry: &Entry, here: &mut Vec<String>, inside: bool, root_keys: &[String], f: &mut impl FnMut(&Entry, &str)) {
	let key = key_of(here);
	let synced = inside || root_keys.contains(&key);
	if synced && !here.is_empty() {
		f(entry, &key);
	}
	for child in &entry.children {
		here.push(child.name.clone());
		visit_synced(child, here, synced, root_keys, f);
		here.pop();
	}
}

struct Collector<'a> {
	dir: &'a Path,
	files: HashMap<PathBuf, Res<Vec<u8>>>,
	reuse: Option<Reuse>,
	root_keys: Vec<String>,
	read_only: Vec<Vec<String>>,
	mapped: BTreeMap<String, String>,
	nodes: Nodes,
	places: BTreeMap<String, Place>,
}

impl Collector<'_> {
	// A file's bytes, from read_synced (or the disk, for one it didn't list).
	fn bytes(&mut self, file: &Path) -> Res<Vec<u8>> {
		self.files.remove(file).unwrap_or_else(|| read(&self.dir.join(file)))
	}

	// Invalid UTF-8 becomes U+FFFD, as read_text does.
	fn text(&mut self, file: &Path) -> Res<String> {
		let bytes = self.bytes(file)?;
		Ok(String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
	}

	fn json(&mut self, file: &Path) -> Res<Value> {
		serde_json::from_slice(&self.bytes(file)?).map_err(|e| crate::util::Error(format!("{}: {e}", file.display())))
	}

	fn collect(&mut self, node: &Entry, here: &mut Vec<String>, inside: bool, model: Option<&Value>) -> Res<()> {
		let key = key_of(here);
		// only synced instances need their files' contents; the rest are walked for the roots below them
		let synced = inside || self.root_keys.contains(&key);
		let files = files_of(node);
		let reused = match (&self.reuse, model) {
			(Some(reuse), None) if synced && !here.is_empty() => reuse.node(&key, node, &files).cloned(),
			_ => None,
		};
		let read = synced && reused.is_none();

		let mut kind = if model.is_some() { Kind::Json } else { Kind::Project };
		let mut place = Place { files, ..Place::default() };
		let mut source = None;
		let mut run_context = None;
		let mut error = None;
		let mut meta = None;
		let mut own_model = None;
		let mut properties = Map::new();
		let mut model_file = None;

		// the project file names it: a pull never turns it into a directory
		place.pinned = self.mapped.contains_key(&key);
		if let (Some(dir), None) = (&node.dir, model) {
			kind = Kind::Dir;
			place.dir = Some(dir.clone());
		}

		for file in &node.file_paths {
			let file = normalize(file);
			let base = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
			let parent = || file.parent().map(Path::to_path_buf);
			if base.ends_with(".meta.json") {
				if read {
					meta = Some(self.json(&file)?);
				}
				if base.starts_with("init.") {
					kind = Kind::Dir;
					place.dir = parent();
				}
				place.meta = Some(file);
			} else if base.ends_with(".model.json") {
				if read {
					own_model = Some(self.json(&file)?);
				}
				kind = Kind::Json;
				place.file = Some(file);
			} else if base.ends_with(".project.json") {
				// a nested project: its instances come through the layout
			} else if let Some(stem) = base.strip_suffix(".luau").or_else(|| base.strip_suffix(".lua")) {
				if read {
					// as Rojo builds it: a built copy then holds the same text
					source = Some(self.text(&file)?.replace("\r\n", "\n"));
				}
				if base.starts_with("init.") {
					kind = Kind::Init;
					place.dir = parent();
				} else {
					kind = Kind::Script;
				}
				if stem.ends_with(".server") {
					run_context = Some("Server");
				} else if stem.ends_with(".client") {
					run_context = Some("Client");
				}
				place.file = Some(file);
			} else if base.ends_with(".rbxm") || base.ends_with(".rbxmx") {
				kind = Kind::Model;
				model_file = Some(file.clone());
				place.file = Some(file);
			} else if base.ends_with(".json") {
				kind = Kind::Generated;
				if read {
					source = Some(format!("return {}", luau_literal(&self.json(&file)?))); // no trailing newline, like Rojo
				}
				place.file = Some(file);
			} else if base.ends_with(".txt") {
				kind = Kind::Generated;
				if read {
					properties.insert("Value".into(), Value::String(self.text(&file)?));
				}
				place.file = Some(file);
			} else if base.ends_with(".toml") || base.ends_with(".csv") {
				kind = Kind::Generated;
				error = Some(format!("{}: .toml and .csv files are not synced (`build` still includes them)", file.display()));
				place.file = Some(file);
			}
		}
		let model = own_model.as_ref().or(model);

		if let Some(node) = reused {
			self.nodes.insert(key.clone(), node);
			self.places.insert(key, place.clone());
		} else if synced && !here.is_empty() {
			let ro = self.read_only.iter().any(|r| starts_with(here, r));
			for extra in [meta.as_ref(), model] {
				if let Some(Value::Object(props)) = extra.map(|v| &v["properties"]) {
					properties.extend(props.clone());
				}
			}
			let mut attributes: Option<Map<String, Value>> = None;
			for extra in [meta.as_ref(), model] {
				if let Some(Value::Object(attrs)) = extra.map(|v| &v["attributes"]) {
					attributes.get_or_insert_default().extend(attrs.clone());
				}
			}
			let model_hash = match &model_file {
				Some(file) => Some(sha(self.bytes(file)?)),
				None => None,
			};
			let mut entry = Node {
				path: here.clone(),
				class_name: node.class_name.clone(),
				kind,
				ro,
				source_hash: source.as_ref().map(sha),
				source,
				run_context,
				properties: (!properties.is_empty()).then_some(properties),
				attributes,
				model: model_file.as_ref().map(|f| f.to_string_lossy().into_owned()),
				model_hash,
				error,
				hash: String::new(),
			};
			// the source counts through source_hash, without serializing it again
			let source = entry.source.take();
			entry.hash = sha(serde_json::to_vec(&entry)?);
			entry.source = source;
			self.nodes.insert(key.clone(), Arc::new(entry));
			self.places.insert(key, place.clone());
		}
		// a model file is pushed whole: its children are the file's business
		if kind == Kind::Model {
			return Ok(());
		}
		for child in &node.children {
			let child_model = model.map(|m| {
				m["children"].as_array().and_then(|cs| cs.iter().find(|c| c["name"].as_str() == Some(&child.name))).unwrap_or(&NO_MODEL)
			});
			here.push(child.name.clone());
			self.collect(child, here, synced, child_model)?;
			here.pop();
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;

	#[test]
	fn literals() {
		let value = serde_json::json!({ "end": 1, "b c": [1, 2.5, "x\"\n", null, {}], "a": true });
		assert_eq!(luau_literal(&value), "{\n\ta = true,\n\t[\"b c\"] = {1, 2.5, \"x\\\"\\n\", nil, {\n\t}},\n\t[\"end\"] = 1,\n}");
	}

	fn write(dir: &Path, rel: &str, text: &str) {
		let file = dir.join(rel);
		fs::create_dir_all(file.parent().unwrap()).unwrap();
		fs::write(file, text).unwrap();
	}

	fn hashes(generated: &Generated) -> Vec<(String, String)> {
		generated.nodes.iter().map(|(k, n)| (k.clone(), n.hash.clone())).collect()
	}

	// An incremental regenerate gives the same tree as a full one, and reuses what it can.
	#[test]
	fn incremental_matches_full() {
		let dir = std::env::temp_dir().join(format!("studio-sync-tree-{}", std::process::id()));
		let _ = fs::remove_dir_all(&dir);
		write(&dir, "studio-sync.json", r#"{ "placeIds": [1] }"#);
		write(
			&dir,
			"default.project.json",
			r#"{ "name": "t", "tree": { "$className": "DataModel", "ReplicatedStorage": { "Shared": { "$path": "src" } } } }"#,
		);
		for (rel, text) in [
			("src/a.luau", "return 1"),
			("src/b/init.luau", "return {}"),
			("src/b/c.server.luau", "print(1)"),
			("src/d.luau", "return 'd'"),
			("src/d.meta.json", r#"{ "attributes": { "x": 1 } }"#),
			("src/m.model.json", r#"{ "className": "Folder", "children": [{ "name": "v", "className": "IntValue" }] }"#),
			("src/t.txt", "hi"),
			("src/data.json", r#"{ "k": [1, 2] }"#),
		] {
			write(&dir, rel, text);
		}
		let config = Config::load(&dir).unwrap();
		let before = generate(&dir, &config, None).unwrap();

		write(&dir, "src/a.luau", "return 2");
		write(&dir, "src/b/c.server.luau", "print(2)");
		write(&dir, "src/d.meta.json", r#"{ "attributes": { "x": 2 } }"#);
		write(&dir, "src/t.txt", "ho");
		write(&dir, "src/data.json", r#"{ "k": [3] }"#);
		write(&dir, "src/m.model.json", r#"{ "className": "Folder", "children": [{ "name": "w", "className": "IntValue" }] }"#);
		write(&dir, "src/e/f.luau", "return 'f'");
		fs::remove_file(dir.join("src/b/init.luau")).unwrap();
		let changed = [
			"src/a.luau",
			"src/b/c.server.luau",
			"src/d.meta.json",
			"src/t.txt",
			"src/data.json",
			"src/m.model.json",
			"src/e",
			"src/b/init.luau",
		];
		let reuse =
			Reuse { nodes: before.nodes.clone(), places: before.places.clone(), changed: changed.iter().map(PathBuf::from).collect() };
		let incremental = generate(&dir, &config, Some(reuse)).unwrap();
		let full = generate(&dir, &config, None).unwrap();
		fs::remove_dir_all(&dir).unwrap();

		assert_eq!(hashes(&incremental), hashes(&full));
		let unchanged = key_of(&["ReplicatedStorage".into(), "Shared".into()]);
		assert!(Arc::ptr_eq(&before.nodes[&unchanged], &incremental.nodes[&unchanged]));
		let edited = key_of(&["ReplicatedStorage".into(), "Shared".into(), "a".into()]);
		assert_eq!(incremental.nodes[&edited].source.as_deref(), Some("return 2"));
	}
}
