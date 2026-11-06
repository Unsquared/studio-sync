// Small helpers shared by the daemon and the CLI: errors that name their file,
// file operations, hashes, and timestamps.

use serde_json::Value;
use sha1::{Digest, Sha1};
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug)]
pub struct Error(pub String);

pub type Res<T> = Result<T, Error>;

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		f.write_str(&self.0)
	}
}

impl From<String> for Error {
	fn from(message: String) -> Self {
		Error(message)
	}
}

impl From<&str> for Error {
	fn from(message: &str) -> Self {
		Error(message.to_string())
	}
}

impl From<serde_json::Error> for Error {
	fn from(e: serde_json::Error) -> Self {
		Error(e.to_string())
	}
}

#[macro_export]
macro_rules! bail {
	($($arg:tt)*) => {
		return Err($crate::util::Error(format!($($arg)*)))
	};
}

fn at(path: &Path) -> impl Fn(io::Error) -> Error + '_ {
	move |e| Error(format!("{}: {e}", path.display()))
}

pub fn read(path: &Path) -> Res<Vec<u8>> {
	fs::read(path).map_err(at(path))
}

// Invalid UTF-8 becomes U+FFFD, as Node's readFileSync(file, "utf8") does.
pub fn read_text(path: &Path) -> Res<String> {
	Ok(String::from_utf8(read(path)?).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

pub fn read_json(path: &Path) -> Res<Value> {
	serde_json::from_slice(&read(path)?).map_err(|e| Error(format!("{}: {e}", path.display())))
}

pub fn write(path: &Path, data: impl AsRef<[u8]>) -> Res<()> {
	fs::write(path, data).map_err(at(path))
}

pub fn mkdirs(path: &Path) -> Res<()> {
	fs::create_dir_all(path).map_err(at(path))
}

pub fn rename(from: &Path, to: &Path) -> Res<()> {
	fs::rename(from, to).map_err(|e| Error(format!("{} -> {}: {e}", from.display(), to.display())))
}

// rm -rf: a missing path is fine.
pub fn remove(path: &Path) -> Res<()> {
	let result = match fs::symlink_metadata(path) {
		Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
		Ok(_) => fs::remove_file(path),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
		Err(e) => Err(e),
	};
	result.map_err(at(path))
}

// cp -r: a file or a whole directory.
pub fn copy(from: &Path, to: &Path) -> Res<()> {
	if from.is_dir() {
		mkdirs(to)?;
		for entry in fs::read_dir(from).map_err(at(from))? {
			let entry = entry.map_err(at(from))?;
			copy(&entry.path(), &to.join(entry.file_name()))?;
		}
		Ok(())
	} else {
		fs::copy(from, to).map(|_| ()).map_err(|e| Error(format!("{} -> {}: {e}", from.display(), to.display())))
	}
}

// Every file under `dir`, recursively.
pub fn files_under(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Res<()> {
	for entry in fs::read_dir(dir).map_err(at(dir))? {
		let path = entry.map_err(at(dir))?.path();
		if path.is_dir() {
			files_under(&path, out)?;
		} else {
			out.push(path);
		}
	}
	Ok(())
}

pub fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// The short content hash the plugin and the daemon compare: sha1, 16 hex digits.
pub fn sha(data: impl AsRef<[u8]>) -> String {
	hex(&Sha1::digest(data.as_ref())[..8])
}

pub fn random_hex(bytes: usize) -> String {
	let mut buf = vec![0u8; bytes];
	getrandom::fill(&mut buf).expect("the OS random source");
	hex(&buf)
}

// An ISO time that is also a file name: 2026-11-03T12-34-56-789Z.
pub fn stamp() -> String {
	chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S-%3fZ").to_string()
}

pub fn now_ms() -> u64 {
	chrono::Utc::now().timestamp_millis().max(0) as u64
}

pub fn iso_now() -> String {
	chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

// A path with forward slashes, the way manifests and the project file store it.
pub fn slashes(path: &Path) -> String {
	path.to_string_lossy().replace('\\', "/")
}
