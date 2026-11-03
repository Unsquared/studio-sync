// The daemon log: one file, rolled over at 5 MB so a daemon that runs for months stays small.

use crate::paths::log_file;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

const MAX_BYTES: u64 = 5 * 1024 * 1024;
static FILE: OnceLock<Mutex<File>> = OnceLock::new();

pub fn to_file() {
	let path = log_file();
	if let Some(dir) = path.parent() {
		let _ = fs::create_dir_all(dir);
	}
	if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
		let mut old = path.clone().into_os_string();
		old.push(".1");
		let _ = fs::rename(&path, old);
	}
	if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
		let _ = FILE.set(Mutex::new(file));
	}
}

pub fn write(message: &str) {
	let line = format!("{} {message}\n", chrono::Local::now().format("%H:%M:%S"));
	match FILE.get() {
		Some(file) => {
			let _ = file.lock().unwrap().write_all(line.as_bytes());
		}
		None => print!("{line}"),
	}
}

#[macro_export]
macro_rules! log {
	($($arg:tt)*) => {
		$crate::log::write(&format!($($arg)*))
	};
}
