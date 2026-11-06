// events.jsonl: what the CLI and the daemon did, one JSON object a line, to analyze problems after
// they happened. They can't be reproduced later, so this is always on. It stays on this machine
// and small (two files of at most EVENTS_MAX bytes). A failed write never matters to the caller.

use crate::paths::events_file;
use crate::util::iso_now;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

const EVENTS_MAX: u64 = 20 * 1024 * 1024;
// How many names one event carries
const NAMES_MAX: usize = 20;

// Only the daemon rolls the file over: a CLI could rename it under another process's write.
static ROLLS: AtomicBool = AtomicBool::new(false);

pub fn rolls_over() {
	ROLLS.store(true, Ordering::Relaxed);
}

// Appends { t, ev, ...fields }, in one write so that the processes' lines don't mix.
pub fn record(event: &str, fields: Value) {
	let mut line = json!({ "t": iso_now(), "ev": event });
	if let (Some(line), Value::Object(fields)) = (line.as_object_mut(), fields) {
		line.extend(fields);
	}
	let file = events_file();
	if ROLLS.load(Ordering::Relaxed) && fs::metadata(&file).is_ok_and(|m| m.len() > EVENTS_MAX) {
		let _ = fs::rename(&file, file.with_extension("jsonl.1"));
	}
	if let Ok(mut out) = OpenOptions::new().create(true).append(true).open(&file) {
		let _ = out.write_all(format!("{line}\n").as_bytes());
	}
}

// Some of the names, and how many there were.
pub fn names<'a>(all: impl ExactSizeIterator<Item = &'a Vec<String>>) -> Value {
	let count = all.len();
	json!({ "count": count, "first": all.take(NAMES_MAX).map(|path| path.join(".")).collect::<Vec<_>>() })
}
