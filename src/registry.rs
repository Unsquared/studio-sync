// projects.json: the registered projects, the place ids a checkout claimed, and
// the open copies. The CLI writes it; the daemon watches it.

use crate::config::CONFIG_FILE;
use crate::paths::{STATE_DIR, projects_file};
use crate::util::{Error, Res, mkdirs, read, rename, write};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Default, Serialize, Deserialize)]
pub struct Registry {
	#[serde(default)]
	pub projects: Vec<PathBuf>,
	#[serde(default)]
	pub claims: BTreeMap<String, PathBuf>,
	#[serde(default)]
	pub sessions: BTreeMap<String, Session>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
	pub dir: PathBuf,
	pub file: PathBuf,
	#[serde(rename = "createdAt")]
	pub created_at: String,
	// When its build started, in ms: a model file not changed since is what the copy holds.
	#[serde(rename = "builtAt", default)]
	pub built_at: u64,
}

impl Registry {
	// No file is an empty registry. A file that does not parse is an error, never an empty
	// registry: writing that back would forget every project.
	pub fn read() -> Res<Registry> {
		let file = projects_file();
		match read(&file) {
			Ok(data) => serde_json::from_slice(&data).map_err(|e| Error(format!("{}: {e}", file.display()))),
			Err(_) if !file.exists() => Ok(Registry::default()),
			Err(e) => Err(e),
		}
	}

	pub fn add(&mut self, dir: &Path) {
		if !self.projects.iter().any(|p| p == dir) {
			self.projects.push(dir.to_path_buf());
		}
	}

	// Drops projects whose directory or config is gone, then replaces the file in one rename,
	// so the daemon never reads half a file.
	pub fn write(mut self) -> Res<()> {
		self.projects.retain(|dir| dir.join(CONFIG_FILE).is_file());
		self.claims.retain(|_, dir| dir.is_dir());
		mkdirs(&STATE_DIR)?;
		let file = projects_file();
		let temp = file.with_extension("json.tmp");
		let mut text = serde_json::to_string_pretty(&self)?;
		text.push('\n');
		write(&temp, text)?;
		rename(&temp, &file)
	}
}
