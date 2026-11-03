// Where studio-sync keeps its state, and the knobs tests override
// (STUDIO_SYNC_HOME, STUDIO_SYNC_PORT) so a second daemon can run beside the real one.

use std::env;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const LABEL: &str = "com.unsquared.studio-sync";
pub const LEGACY_LABELS: &[&str] = &["com.kevin.studio-sync"];

pub static PORT: LazyLock<u16> = LazyLock::new(|| env::var("STUDIO_SYNC_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(34999));

pub static STATE_DIR: LazyLock<PathBuf> = LazyLock::new(|| match env::var_os("STUDIO_SYNC_HOME") {
	Some(dir) => PathBuf::from(dir),
	None => home().join(".config").join("studio-sync"),
});

pub fn home() -> PathBuf {
	env::home_dir().unwrap_or_default()
}

pub fn projects_file() -> PathBuf {
	STATE_DIR.join("projects.json")
}

pub fn staging_root() -> PathBuf {
	STATE_DIR.join("staging")
}

// Studio-side text that disk replaced, files Studio deleted, the losing side of a conflict
pub fn backup_root() -> PathBuf {
	STATE_DIR.join("backups")
}

pub fn copies_dir() -> PathBuf {
	STATE_DIR.join("copies")
}

pub fn log_file() -> PathBuf {
	if env::var_os("STUDIO_SYNC_HOME").is_some() || cfg!(windows) {
		STATE_DIR.join("daemon.log")
	} else {
		home().join("Library").join("Logs").join("studio-sync.log")
	}
}

pub fn local_app_data() -> PathBuf {
	env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home().join("AppData").join("Local"))
}

pub fn plugin_dir() -> PathBuf {
	if cfg!(windows) { local_app_data().join("Roblox").join("Plugins") } else { home().join("Documents").join("Roblox").join("Plugins") }
}

// The real path, without the \\?\ prefix Windows puts on canonical paths.
pub fn real(path: &Path) -> io::Result<PathBuf> {
	let real = fs::canonicalize(path)?;
	#[cfg(windows)]
	if let Some(plain) = real.to_str().and_then(|s| s.strip_prefix(r"\\?\"))
		&& !plain.starts_with(r"UNC\")
	{
		return Ok(PathBuf::from(plain));
	}
	Ok(real)
}

// `.` and `..` resolved without touching the disk, like Node's path.normalize.
pub fn normalize(path: &Path) -> PathBuf {
	let mut out = PathBuf::new();
	for component in path.components() {
		match component {
			Component::CurDir => {}
			Component::ParentDir if matches!(out.components().next_back(), Some(Component::Normal(_))) => {
				out.pop();
			}
			other => out.push(other),
		}
	}
	out
}
