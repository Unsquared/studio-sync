// Studio -> disk: what Studio sends to the project's files. Scripts never come back (they
// are edited on disk); models do: a pulled instance becomes a .rbxm, and a model edited in
// Studio is saved over its .rbxm. Disk wins: a save whose base is not what is on disk goes
// to backups instead. Also the backups of what a full sync replaces.

use crate::log;
use crate::paths::{agreed_file, backup_root, normalize};
use crate::project::State;
use crate::tree::{Kind, Place, key_of, starts_with};
use crate::util::{Error, Res, mkdirs, read, read_json, rename, sha, stamp, write};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const RESERVED_SUFFIXES: [&str; 6] = [".server", ".client", ".meta", ".model", ".project", ".plugin"];
// what follows a sibling's name on disk, for the case clash check
const FILE_SUFFIXES: [&str; 8] = [".luau", ".lua", ".meta.json", ".model.json", ".rbxm", ".rbxmx", ".json", ".txt"];

pub fn backup_dir(project: &str, kind: &str) -> PathBuf {
	backup_root().join(project).join(format!("{}-{kind}", stamp()))
}

fn bad_file_name(name: &str) -> bool {
	let lower = name.to_lowercase();
	name.is_empty()
		|| name == "."
		|| name == ".."
		|| name.chars().any(|c| matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c < '\x20')
		|| name.ends_with(' ')
		|| name.ends_with('.')
		|| lower == "init"
		|| RESERVED_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

fn check_name(name: &str) -> Res<()> {
	if bad_file_name(name) {
		crate::bail!(
			"\"{name}\" can't be a file name (no \\ / : * ? \" < > |, no trailing dot or space, not \"init\" or *.server/.client/.meta)"
		);
	}
	Ok(())
}

// The instance name a file in a directory stands for: "foo" for foo.server.luau.
fn stem(entry: &str) -> &str {
	let lower = entry.to_lowercase();
	for (at, _) in lower.match_indices('.') {
		let rest = &lower[at..];
		let rest = rest.strip_prefix(".server").or_else(|| rest.strip_prefix(".client")).unwrap_or(rest);
		if FILE_SUFFIXES.contains(&rest) {
			return &entry[..at];
		}
	}
	entry
}

// The part of a script file's name after the instance name: ".server.luau" for foo.server.luau.
fn suffix_of(file: &Path, name: &str) -> String {
	let base = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
	base.get(name.len()..).unwrap_or_default().to_string()
}

fn display(path: &[String]) -> String {
	path.join(".")
}

pub struct Writer<'a> {
	dir: &'a Path,
	name: &'a str,
	st: &'a mut State,
}

impl<'a> Writer<'a> {
	pub fn new(dir: &'a Path, name: &'a str, st: &'a mut State) -> Self {
		Writer { dir, name, st }
	}

	fn abs(&self, rel: &Path) -> Res<PathBuf> {
		let full = normalize(&self.dir.join(rel));
		if full == self.dir || !full.starts_with(self.dir) {
			crate::bail!("{} is outside the project", rel.display());
		}
		Ok(full)
	}

	fn place(&self, path: &[String]) -> Option<Place> {
		let key = key_of(path);
		self.st.pending_places.get(&key).or_else(|| self.st.places.get(&key)).cloned()
	}

	fn kind(&self, path: &[String]) -> Option<Kind> {
		self.st.nodes.get(&key_of(path)).map(|n| n.kind)
	}

	fn writable(&self, path: &[String]) -> Res<()> {
		if !self.st.roots.iter().any(|r| starts_with(path, r)) {
			crate::bail!("{} is outside the synced roots", display(path));
		}
		if self.st.read_only.iter().any(|r| starts_with(path, r)) {
			crate::bail!("{} is read-only (studio-sync.json readOnly)", display(path));
		}
		Ok(())
	}

	// Two siblings whose names differ only in case collide on macOS and Windows.
	fn check_free(&self, dir: &Path, name: &str, except: Option<&Path>) -> Res<()> {
		let lower = name.to_lowercase();
		let abs = self.abs(dir)?;
		for entry in fs::read_dir(&abs).map_err(|e| Error(format!("{}: {e}", abs.display())))? {
			let entry = entry.map_err(|e| Error(e.to_string()))?.file_name();
			let entry = entry.to_string_lossy();
			let rel = dir.join(&*entry);
			if stem(&entry).to_lowercase() == lower && Some(rel.as_path()) != except {
				crate::bail!("{} already exists", rel.display());
			}
		}
		Ok(())
	}

	// The directory a child of `parent` goes in; a single-file script becomes dir/init.
	fn container_dir(&mut self, parent: &[String]) -> Res<PathBuf> {
		let Some(place) = self.place(parent) else {
			crate::bail!("{} is not on disk", display(parent));
		};
		let kind = self.kind(parent);
		let key = key_of(parent);
		if let Some(dir) = &place.dir
			&& (matches!(kind, None | Some(Kind::Dir | Kind::Init)) || self.st.pending_places.contains_key(&key))
		{
			return Ok(dir.clone());
		}
		if let (Some(file), Some(Kind::Script), false) = (&place.file, kind, place.pinned) {
			let name = parent.last().map(String::as_str).unwrap_or_default();
			let dir = file.parent().unwrap_or(Path::new("")).join(name);
			let abs_dir = self.abs(&dir)?;
			fs::create_dir(&abs_dir).map_err(|e| Error(format!("{}: {e}", abs_dir.display())))?;
			let init = dir.join(format!("init{}", suffix_of(file, name)));
			rename(&self.abs(file)?, &self.abs(&init)?)?;
			if let Some(meta) = &place.meta {
				rename(&self.abs(meta)?, &self.abs(&dir.join("init.meta.json"))?)?;
			}
			log!("[{}] {} -> {} (it has children now)", self.name, file.display(), init.display());
			self.st.pending_places.insert(key, Place { dir: Some(dir.clone()), file: Some(init), ..Default::default() });
			return Ok(dir);
		}
		crate::bail!("{} can't hold synced children (it is not a folder or script on disk)", display(parent))
	}

	// Saves a serialized instance as a model file and returns where it went.
	pub fn save_pulled(&mut self, path: &[String], file: Option<&str>, data: &[u8]) -> Res<PathBuf> {
		let rel = match file {
			Some(file) => {
				let mut rel = normalize(Path::new(file));
				if !matches!(rel.extension().and_then(|e| e.to_str()), Some("rbxm" | "rbxmx")) {
					rel.as_mut_os_string().push(".rbxm");
				}
				rel
			}
			None => {
				let key = key_of(path);
				match (self.kind(path), self.st.places.get(&key).and_then(|p| p.file.clone())) {
					(Some(Kind::Model), Some(file)) => file,
					(Some(kind), _) => crate::bail!(
						"{} is a {} on disk; scripts and folders sync by themselves, pull what is inside it",
						display(path),
						serde_json::to_value(kind)?.as_str().unwrap_or_default()
					),
					(None, _) => {
						self.writable(path)?;
						let (leaf, parent) = path.split_last().ok_or("an empty path")?;
						check_name(leaf)?;
						let dir = self.container_dir(parent)?;
						self.check_free(&dir, leaf, None)?;
						dir.join(format!("{leaf}.rbxm"))
					}
				}
			}
		};
		let full = self.abs(&rel)?;
		mkdirs(full.parent().unwrap_or(self.dir))?;
		write(&full, data)?;
		log!("[{}] <- Studio: saved {} to {} ({} bytes)", self.name, display(path), rel.display(), data.len());
		Ok(rel)
	}

	// Saves a model from Studio over its file, or says which side wins. `place` is the Studio's
	// place (or copy); `mode` is what Studio is doing:
	// - "connect": Studio's model as it is at a connect. Compared with the version both last
	//   agreed on, only the side that changed since wins: "load" (the file changed, as after a
	//   git pull) or "saved" (Studio changed). When both changed, or nothing is agreed yet,
	//   Studio wins and the file goes to backups first.
	// - "edit": Studio edited the model. `base` is the file hash Studio's version comes from; a
	//   file that is not that is backed up first.
	// - "loaded": Studio just loaded the file whose hash is `base`. Nothing is written; its
	//   serialization is recorded (it differs from the file: Studio gives the scripts inside new
	//   ScriptGuids).
	// Answers `{ ok, action, modelHash, replaced? }`; Studio keeps modelHash, the file's hash.
	pub fn save_model(&mut self, path: &[String], place: &str, mode: &str, base: &str, data: &[u8]) -> Res<Value> {
		if !self.st.config.writeback {
			crate::bail!("writeback is off in studio-sync.json");
		}
		self.writable(path)?;
		let (Some(Kind::Model), Some(file)) = (self.kind(path), self.place(path).and_then(|p| p.file)) else {
			crate::bail!("{} is not a model file", display(path));
		};
		if file.extension().is_none_or(|e| e != "rbxm") {
			crate::bail!("{} is XML; Studio saves only .rbxm (pull it to a .rbxm to keep its edits)", file.display());
		}
		let full = self.abs(&file)?;
		let old = read(&full)?;
		let (on_disk, studio) = (sha(&old), sha(data));
		let key = file.to_string_lossy().replace('\\', "/");
		let last = self.agreed().0.get(place).and_then(|m| m.get(&key)).cloned();

		let answer = |action: &str, hash: &str| json!({ "ok": true, "action": action, "modelHash": hash });
		if mode == "loaded" {
			self.agree(place, key, [base.to_string(), studio])?;
			return Ok(answer("recorded", base));
		}
		if on_disk == studio {
			self.agree(place, key, [on_disk.clone(), studio])?;
			return Ok(answer("none", &on_disk));
		}
		let file_changed = match (&last, mode) {
			(_, "edit") => on_disk != base,
			(Some([agreed_file, _]), _) => *agreed_file != on_disk,
			(None, _) => true, // nothing agreed: the file may hold work Studio lacks
		};
		if mode == "connect" && last.as_ref().is_some_and(|[_, agreed_studio]| *agreed_studio == studio) {
			// Studio is as agreed: the file is newer, or differs only by ScriptGuids
			return Ok(answer(if file_changed { "load" } else { "none" }, &on_disk));
		}
		let mut replaced = None;
		if file_changed {
			let dir = backup_dir(self.name, "replaced");
			let saved = dir.join(&file);
			mkdirs(saved.parent().unwrap_or(&dir))?;
			write(&saved, &old)?;
			let both = if last.is_some() || mode == "edit" { "changed in Studio and on disk" } else { "first connect" };
			log!("[{}] {} ({both}): Studio's version wins; the file is backed up in {}", self.name, file.display(), dir.display());
			replaced = Some(dir);
		}
		write(&full, data)?;
		log!("[{}] <- Studio: {} ({} bytes)", self.name, file.display(), data.len());
		self.agree(place, key, [studio.clone(), studio.clone()])?;
		let mut result = answer("saved", &studio);
		result["replaced"] = json!(replaced);
		Ok(result)
	}

	fn agreed(&mut self) -> &mut Agreed {
		let name = self.name;
		self.st
			.agreed
			.get_or_insert_with(|| read_json(&agreed_file(name)).ok().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
	}

	fn agree(&mut self, place: &str, file: String, hashes: [String; 2]) -> Res<()> {
		let name = self.name;
		let agreed = self.agreed();
		if agreed.0.get(place).and_then(|m| m.get(&file)) == Some(&hashes) {
			return Ok(());
		}
		agreed.0.entry(place.to_string()).or_default().insert(file, hashes);
		let target = agreed_file(name);
		mkdirs(target.parent().unwrap_or(&target))?;
		write(&target, serde_json::to_vec(&agreed.0)?)
	}
}

// By place, then model file: the hash of the file and of Studio's serialization when the two
// last matched (they differ when Studio has loaded the file: new ScriptGuids).
#[derive(Default, Serialize, Deserialize)]
pub struct Agreed(BTreeMap<String, BTreeMap<String, [String; 2]>>);

#[derive(Deserialize)]
pub struct BackupItem {
	path: Vec<String>,
	source: Option<String>,
	rbxm: Option<String>,
}

// Studio's side of what a full sync is about to overwrite or remove.
pub fn save_backup(project: &str, items: &[BackupItem]) -> Res<PathBuf> {
	let dir = backup_dir(project, "studio");
	for item in items {
		let mut target = dir.clone();
		for name in &item.path {
			target.push(name.replace(|c: char| matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c < '\x20', "_"));
		}
		target.as_mut_os_string().push(if item.source.is_some() { ".luau" } else { ".rbxm" });
		mkdirs(target.parent().unwrap_or(&dir))?;
		if let Some(source) = &item.source {
			write(&target, source)?;
		} else if let Some(rbxm) = &item.rbxm {
			write(&target, BASE64.decode(rbxm).map_err(|e| Error(e.to_string()))?)?;
		}
	}
	log!("[{project}] backed up {} Studio item(s) the sync replaced: {}", items.len(), dir.display());
	Ok(dir)
}
