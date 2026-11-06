// The daemon log: one file, rolled over at 5 MB so a daemon that runs for months stays small.

use crate::paths::log_file;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};

const MAX_BYTES: u64 = 5 * 1024 * 1024;

struct Output {
	file: File,
	bytes: u64,
}

static OUTPUT: OnceLock<Mutex<Output>> = OnceLock::new();

fn open() -> Option<Output> {
	let path = log_file();
	if let Some(dir) = path.parent() {
		let _ = fs::create_dir_all(dir);
	}
	if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
		let mut old = path.clone().into_os_string();
		old.push(".1");
		let _ = fs::rename(&path, old);
	}
	let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
	let bytes = file.metadata().map_or(0, |m| m.len());
	Some(Output { file, bytes })
}

pub fn to_file() {
	if let Some(output) = open() {
		let _ = OUTPUT.set(Mutex::new(output));
	}
}

pub fn write(message: &str) {
	let line = format!("{} {message}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
	match OUTPUT.get() {
		Some(output) => {
			let mut output = output.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
			if output.bytes > MAX_BYTES
				&& let Some(fresh) = open()
			{
				*output = fresh;
			}
			output.bytes += line.len() as u64;
			let _ = output.file.write_all(line.as_bytes());
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
