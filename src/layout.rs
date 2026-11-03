// Which instance each file and directory of a project is, by Rojo's default sync rules:
// the same tree `rojo sourcemap --include-non-scripts` prints, without running Rojo.
// Paths are relative to the project dir. tree.rs reads the files themselves.

use crate::paths::normalize;
use crate::util::{Res, read_json, read_text};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
#[cfg_attr(test, derive(serde::Deserialize))]
#[cfg_attr(test, serde(rename_all = "camelCase"))]
pub struct Entry {
	pub name: String,
	pub class_name: String,
	#[cfg_attr(test, serde(default))]
	pub file_paths: Vec<PathBuf>,
	#[cfg_attr(test, serde(default))]
	pub children: Vec<Entry>,
	// the directory it is, when it is one
	#[cfg_attr(test, serde(skip))]
	pub dir: Option<PathBuf>,
}

impl Entry {
	fn new(name: &str, class_name: &str) -> Entry {
		Entry { name: name.to_string(), class_name: class_name.to_string(), file_paths: Vec::new(), children: Vec::new(), dir: None }
	}
}

// What a file is, by its name.
#[derive(Clone, Copy)]
enum File {
	Script(&'static str),
	// a ModuleScript, StringValue, ... made from the file's content
	Value(&'static str),
	Meta,
	ModelJson,
	Project,
	Rbxmx,
	Rbxm,
}

pub fn read(dir: &Path, project_file: &str) -> Res<Entry> {
	let project = read_json(&dir.join(project_file))?;
	let reader = Reader { dir, legacy: project["emitLegacyScripts"].as_bool().unwrap_or(true) };
	reader.project(Path::new(project_file), &project)
}

struct Reader<'a> {
	dir: &'a Path,
	// LocalScript for .client files, as Rojo does unless the project sets emitLegacyScripts: false
	legacy: bool,
}

impl Reader<'_> {
	// The instance name and kind of a file, or None for a file Rojo ignores.
	fn file<'n>(&self, file_name: &'n str) -> Option<(&'n str, File)> {
		let script = |suffix: &str| [".luau", ".lua"].iter().find_map(|ext| file_name.strip_suffix(ext)?.strip_suffix(suffix));
		if let Some(name) = script(".server") {
			return Some((name, File::Script("Script")));
		}
		if let Some(name) = script(".client") {
			return Some((name, File::Script(if self.legacy { "LocalScript" } else { "Script" })));
		}
		if let Some(name) = script("") {
			return Some((name, File::Script("ModuleScript")));
		}
		let suffixes: [(&str, File); 9] = [
			(".meta.json", File::Meta),
			(".model.json", File::ModelJson),
			(".project.json", File::Project),
			(".json", File::Value("ModuleScript")),
			(".toml", File::Value("ModuleScript")),
			(".txt", File::Value("StringValue")),
			(".csv", File::Value("LocalizationTable")),
			(".rbxmx", File::Rbxmx),
			(".rbxm", File::Rbxm),
		];
		suffixes.into_iter().find_map(|(suffix, kind)| Some((file_name.strip_suffix(suffix)?, kind)))
	}

	// A project file's tree, named by the project.
	fn project(&self, file: &Path, project: &Value) -> Res<Entry> {
		let name = project["name"].as_str().unwrap_or_default();
		let base = file.parent().unwrap_or(Path::new(""));
		let Some(mut entry) = self.node(name, &project["tree"], base)? else {
			crate::bail!("{}: the root's $path does not exist", file.display());
		};
		entry.file_paths.push(file.to_path_buf());
		Ok(entry)
	}

	// A node of a project tree: its $path's instance, if any, with the declared children over it.
	// None for an optional $path that does not exist.
	fn node(&self, name: &str, body: &Value, base: &Path) -> Res<Option<Entry>> {
		let class_name = body["$className"].as_str();
		let mut entry = match &body["$path"] {
			Value::Null => Entry::new(name, class_name.unwrap_or(name)), // a service, named for its class
			path => {
				let (rel, optional) = match path {
					Value::String(rel) => (rel.as_str(), false),
					_ => (path["optional"].as_str().unwrap_or_default(), true),
				};
				let rel = normalize(&base.join(rel));
				if !self.dir.join(&rel).exists() {
					if optional {
						return Ok(None);
					}
					crate::bail!("{name}: $path {} does not exist", rel.display());
				}
				let Some(mut entry) = self.path(&rel)? else {
					crate::bail!("{name}: $path {} is no kind of file Rojo syncs", rel.display());
				};
				entry.name = name.to_string();
				if let Some(class_name) = class_name {
					entry.class_name = class_name.to_string();
				}
				entry
			}
		};
		for (key, child) in body.as_object().into_iter().flatten() {
			if key.starts_with('$') || !child.is_object() {
				continue;
			}
			if let Some(child) = self.node(key, child, base)? {
				entry.children.retain(|c| c.name != child.name);
				entry.children.push(child);
			}
		}
		Ok(Some(entry))
	}

	// The instance at a file or directory, or None for a file Rojo ignores.
	fn path(&self, rel: &Path) -> Res<Option<Entry>> {
		if self.dir.join(rel).is_dir() {
			return self.directory(rel).map(Some);
		}
		self.file_entry(rel, |meta| self.dir.join(meta).is_file())
	}

	// The instance a file is; `exists` says whether a sibling file (its .meta.json) is there.
	fn file_entry(&self, rel: &Path, exists: impl Fn(&Path) -> bool) -> Res<Option<Entry>> {
		let full = self.dir.join(rel);
		let file_name = rel.file_name().and_then(|n| n.to_str()).unwrap_or_default();
		let Some((name, kind)) = self.file(file_name) else { return Ok(None) };
		let mut entry = match kind {
			File::Meta => return Ok(None), // part of its sibling
			File::Project => return self.project(rel, &read_json(&full)?).map(Some),
			File::Script(class_name) | File::Value(class_name) => Entry::new(name, class_name),
			File::ModelJson => model_json(name, &read_json(&full)?),
			File::Rbxmx => Entry::new(name, &rbxmx_class(&read_text(&full)?)),
			// the plugin takes the class from the file itself; reading it here would mean parsing the binary format
			File::Rbxm => Entry::new(name, "Model"),
		};
		entry.file_paths.push(rel.to_path_buf());
		if matches!(kind, File::Script(_) | File::Value(_)) {
			let meta = rel.with_file_name(format!("{name}.meta.json"));
			if exists(&meta) {
				entry.file_paths.push(meta);
			}
		}
		Ok(Some(entry))
	}

	// A Folder of its entries; an init script makes it that script, init.meta.json can set its class,
	// and a default.project.json makes it that project.
	fn directory(&self, rel: &Path) -> Res<Entry> {
		let full = self.dir.join(rel);
		// (name, is a directory): the listing says which, so no entry needs a stat of its own
		let mut items: Vec<(String, bool)> = fs::read_dir(&full)
			.map_err(|e| crate::util::Error(format!("{}: {e}", full.display())))?
			.filter_map(Result::ok)
			.map(|e| {
				let is_dir = match e.file_type() {
					Ok(t) if t.is_symlink() => e.path().is_dir(),
					Ok(t) => t.is_dir(),
					Err(_) => false,
				};
				(e.file_name().to_string_lossy().into_owned(), is_dir)
			})
			.collect();
		items.sort();
		let files: HashSet<&str> = items.iter().filter(|(_, is_dir)| !is_dir).map(|(n, _)| n.as_str()).collect();
		if files.contains("default.project.json") {
			let project = rel.join("default.project.json");
			return self.project(&project, &read_json(&self.dir.join(&project))?);
		}
		let name = rel.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
		let mut entry = Entry::new(&name, "Folder");
		entry.dir = Some(rel.to_path_buf());
		let sibling = |meta: &Path| meta.file_name().and_then(|n| n.to_str()).is_some_and(|n| files.contains(n));
		let mut meta = None;
		for (file_name, is_dir) in &items {
			let child = rel.join(file_name);
			if *is_dir {
				entry.children.push(self.directory(&child)?);
				continue;
			}
			match self.file(file_name) {
				Some(("init", File::Script(class_name))) => {
					entry.class_name = class_name.to_string();
					entry.file_paths.insert(0, child);
				}
				Some(("init", File::Meta)) => meta = Some(child),
				_ => entry.children.extend(self.file_entry(&child, sibling)?),
			}
		}
		if let Some(meta) = meta {
			if entry.file_paths.is_empty()
				&& let Some(class_name) = read_json(&self.dir.join(&meta))?["className"].as_str()
			{
				entry.class_name = class_name.to_string();
			}
			entry.file_paths.push(meta);
		}
		Ok(entry)
	}
}

// The instance a .model.json describes, and its children.
fn model_json(name: &str, model: &Value) -> Entry {
	let field = |key: &str, legacy: &str| model.get(key).or_else(|| model.get(legacy)).cloned().unwrap_or_default();
	let mut entry = Entry::new(name, field("className", "ClassName").as_str().unwrap_or("Folder"));
	for child in field("children", "Children").as_array().into_iter().flatten() {
		let child_name = child.get("name").or_else(|| child.get("Name")).and_then(Value::as_str).unwrap_or_default();
		entry.children.push(model_json(child_name, child));
	}
	entry
}

// The class of an .rbxmx's first instance.
fn rbxmx_class(xml: &str) -> String {
	let Some(at) = xml.find("<Item class=\"") else { return "Model".into() };
	let rest = &xml[at + "<Item class=\"".len()..];
	rest[..rest.find('"').unwrap_or(0)].to_string()
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::process::Command;

	// Both trees in a comparable form: children by name, real file paths, sorted. A model file's
	// class and contents are left out: the plugin reads them from the file.
	fn comparable(entry: &mut Entry, dir: &Path) {
		let model = |f: &PathBuf| f.extension().is_some_and(|e| e == "rbxm" || e == "rbxmx");
		if entry.file_paths.iter().any(model) {
			entry.class_name = "Model".into();
			entry.children.clear();
		}
		entry.file_paths = entry.file_paths.iter().map(|f| crate::paths::real(&dir.join(f)).unwrap_or_else(|_| dir.join(f))).collect();
		entry.file_paths.sort();
		entry.children.iter_mut().for_each(|c| comparable(c, dir));
		entry.children.sort_by(|a, b| (&a.name, &a.class_name).cmp(&(&b.name, &b.class_name)));
	}

	fn differences(ours: &Entry, rojo: &Entry, path: &str, out: &mut Vec<String>) {
		let here = format!("{path}/{}", rojo.name);
		if ours.class_name != rojo.class_name || ours.file_paths != rojo.file_paths {
			out.push(format!("{here}: ours {} {:?}, Rojo's {} {:?}", ours.class_name, ours.file_paths, rojo.class_name, rojo.file_paths));
		}
		let names = |e: &Entry| e.children.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
		if names(ours) != names(rojo) {
			out.push(format!("{here}: children ours {:?}, Rojo's {:?}", names(ours), names(rojo)));
			return;
		}
		for (a, b) in ours.children.iter().zip(&rojo.children) {
			differences(a, b, &here, out);
		}
	}

	// Where the reader and `rojo sourcemap` disagree about a project.
	pub fn compare(dir: &Path, project_file: &str) -> Vec<String> {
		let output = Command::new("rojo").args(["sourcemap", "--include-non-scripts", project_file]).current_dir(dir).output().unwrap();
		assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
		let mut theirs: Entry = serde_json::from_slice(&output.stdout).unwrap();
		let mut ours = read(dir, project_file).unwrap();
		comparable(&mut theirs, dir);
		comparable(&mut ours, dir);
		let mut out = Vec::new();
		differences(&ours, &theirs, "", &mut out);
		out
	}

	fn write(dir: &Path, rel: &str, text: &str) {
		let file = dir.join(rel);
		fs::create_dir_all(file.parent().unwrap()).unwrap();
		fs::write(file, text).unwrap();
	}

	// Every rule the reader knows, against Rojo itself (needs `rojo` on PATH).
	#[test]
	fn matches_rojo() {
		let dir = std::env::temp_dir().join(format!("studio-sync-layout-{}", std::process::id()));
		let _ = fs::remove_dir_all(&dir);
		fs::create_dir_all(&dir).unwrap();
		fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("rokit.toml"), dir.join("rokit.toml")).unwrap();
		let files = [
			("src/a.luau", "return 1"),
			("src/b.server.luau", "print(1)"),
			("src/b.meta.json", r#"{"properties":{"Disabled":true}}"#),
			("src/c.client.luau", "print(2)"),
			("src/d.lua", "return 3"),
			("src/e.module.luau", "return 4"),
			("src/.hidden.luau", "return 5"),
			("src/mod/init.luau", "return {}"),
			("src/mod/init.meta.json", r#"{"properties":{"Archivable":false}}"#),
			("src/mod/child.luau", "return 6"),
			("src/conf/init.meta.json", r#"{"className":"Configuration"}"#),
			("src/conf/v.txt", "x"),
			("src/srv/init.server.luau", "print(3)"),
			("src/cli/init.client.lua", "print(4)"),
			("src/data.json", r#"{"a":1}"#),
			("src/note.txt", "hello"),
			("src/note.meta.json", r#"{"properties":{"Archivable":false}}"#),
			(
				"src/thing.model.json",
				r#"{"className":"Folder","children":[{"name":"P","className":"Part","children":[{"name":"W","className":"Weld"}]}]}"#,
			),
			("src/loc.csv", "Key,Source,Context,Example,es\nk,hi,,,hola\n"),
			("src/settings.toml", "a = 1"),
			("src/README.md", "# not synced"),
			(
				"src/m.rbxmx",
				r#"<roblox version="4"><Item class="Part" referent="0"><Properties><string name="Name">M</string></Properties></Item></roblox>"#,
			),
			("src/side.project.json", r#"{"name":"named","tree":{"$className":"Folder"}}"#),
			("src/pkg/default.project.json", r#"{"name":"lib","tree":{"$path":"src"}}"#),
			("src/pkg/src/init.luau", "return 7"),
			("src/pkg/src/k.luau", "return 8"),
			("src/pkg/wally.toml", "ignored: the directory is its project"),
			("lib/file.luau", "return 'lib'"),
			("nested/n.project.json", r#"{"name":"nested","tree":{"$className":"Folder","inner":{"$path":"inner"}}}"#),
			("nested/inner/x.luau", "return 9"),
		];
		for (rel, text) in files {
			write(&dir, rel, text);
		}
		fs::create_dir_all(dir.join("src/empty")).unwrap();
		let project = |legacy: bool| {
			serde_json::json!({
				"name": "fx",
				"emitLegacyScripts": legacy,
				"tree": {
					"$className": "DataModel",
					"ReplicatedStorage": {
						"src": { "$path": "src" },
						"Lib": { "$path": "lib/file.luau" },
						"Decl": { "$className": "Folder", "Inside": { "$path": "lib" } },
						"Nest": { "$path": "nested/n.project.json" },
						"Opt": { "$path": { "optional": "missing" } },
					},
					"Workspace": { "$properties": { "Gravity": 10 } },
					"ServerScriptService": { "$className": "ServerScriptService", "srv": { "$path": "src/srv" } },
				},
			})
			.to_string()
		};
		write(&dir, "default.project.json", &project(true));
		write(&dir, "modern.project.json", &project(false));
		for file in ["default.project.json", "modern.project.json"] {
			let found = compare(&dir, file);
			assert!(found.is_empty(), "{file}:\n{}", found.join("\n"));
		}
		fs::remove_dir_all(&dir).unwrap();
	}

	// The same check on every registered project of this machine: `cargo test -- --ignored`.
	#[test]
	#[ignore]
	fn matches_rojo_on_registered_projects() {
		let registry = crate::registry::Registry::read().unwrap();
		let mut failed = Vec::new();
		for dir in registry.projects.iter().filter(|d| d.exists()) {
			let config = crate::config::Config::load(dir).unwrap();
			let found = compare(dir, &config.project);
			if !found.is_empty() {
				failed.push(format!("{}:\n  {}", dir.display(), found.join("\n  ")));
			}
		}
		assert!(failed.is_empty(), "{}", failed.join("\n"));
	}
}
