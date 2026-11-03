// What a project watches, and which file events matter. One OS watcher per project
// (FSEvents on macOS, ReadDirectoryChangesW on Windows) covers the project dir; a
// $path outside it gets a watch of its own. Nothing is polled.

use crate::config::{CONFIG_FILE, Config, LOCAL_CONFIG_FILE};
use crate::log;
use crate::paths::{normalize, real};
use crate::tree::{is_package_dir, project_paths};
use crate::util::{Res, read_json};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};

const SKIP: [&str; 3] = [".git", "node_modules", ".DS_Store"];

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
	#[default]
	None,
	Tree,
	Config,
}

pub struct Watch {
	watcher: RecommendedWatcher,
	// the project dir as the OS reports it (macOS reports /private/var for /var)
	base: PathBuf,
	config_files: [PathBuf; 2],
	project_file: PathBuf,
	roots: Vec<PathBuf>,
	outside: Vec<PathBuf>,
}

impl Watch {
	pub fn new(dir: &Path, on_event: impl Fn(notify::Result<Event>) + Send + 'static) -> Res<Watch> {
		let base = real(dir).unwrap_or_else(|_| dir.to_path_buf());
		let mut watcher = notify::recommended_watcher(on_event).map_err(|e| format!("watch {}: {e}", dir.display()))?;
		watcher.watch(&base, RecursiveMode::Recursive).map_err(|e| format!("watch {}: {e}", dir.display()))?;
		Ok(Watch {
			watcher,
			config_files: [base.join(CONFIG_FILE), base.join(LOCAL_CONFIG_FILE)],
			project_file: PathBuf::new(),
			roots: Vec::new(),
			outside: Vec::new(),
			base,
		})
	}

	pub fn classify(&self, event: &Event) -> Change {
		if matches!(event.kind, EventKind::Access(_)) {
			return Change::None;
		}
		if event.need_rescan() {
			return Change::Tree;
		}
		let mut change = Change::None;
		for path in &event.paths {
			if self.config_files.contains(path) {
				return Change::Config;
			}
			if *path == self.project_file || self.roots.iter().any(|root| self.in_root(path, root)) {
				change = Change::Tree;
			}
		}
		change
	}

	// The event's paths relative to the project dir, or None when a full regenerate is due: a
	// rescan, the project file, or a path outside the project dir.
	pub fn touched(&self, event: &Event) -> Option<Vec<PathBuf>> {
		if event.need_rescan() {
			return None;
		}
		event
			.paths
			.iter()
			.map(|path| match path.strip_prefix(&self.base) {
				Ok(rel) if *path != self.project_file => Some(normalize(rel)),
				_ => None,
			})
			.collect()
	}

	// Under a root (and not in .git or node_modules), or a directory that holds one.
	fn in_root(&self, path: &Path, root: &Path) -> bool {
		match path.strip_prefix(root) {
			Ok(rest) => !rest.components().any(|c| SKIP.iter().any(|s| c.as_os_str() == *s)),
			Err(_) => root.starts_with(path),
		}
	}

	// The project file and every $path in it. Packages count only when something uses them.
	pub fn update(&mut self, config: &Config, covers_packages: bool) {
		self.project_file = self.base.join(&config.project);
		let base_dir = self.project_file.parent().unwrap_or(&self.base).to_path_buf();
		let tree = read_json(&self.project_file).map(|p| p["tree"].clone()).unwrap_or_default();
		self.roots = project_paths(&tree)
			.values()
			.map(|p| normalize(&base_dir.join(p)))
			.filter(|full| !is_package_dir(full) || covers_packages)
			.map(|full| if full.starts_with(&self.base) { full } else { real(&full).unwrap_or(full) })
			.collect();
		let outside: Vec<PathBuf> = self.roots.iter().filter(|r| !r.starts_with(&self.base) && r.exists()).cloned().collect();
		for gone in self.outside.iter().filter(|p| !outside.contains(p)) {
			let _ = self.watcher.unwatch(gone);
		}
		for new in outside.iter().filter(|p| !self.outside.contains(p)) {
			if let Err(e) = self.watcher.watch(new, RecursiveMode::Recursive) {
				log!("watch {}: {e}", new.display());
			}
		}
		self.outside = outside;
	}
}
