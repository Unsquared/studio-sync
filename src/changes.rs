// `studio-sync changes`: what happened to this project that an agent's own file edits and git don't
// tell it, such as a model saved from Studio or something gone from Studio. An agent pays for each
// line on every later turn, so it is small by design: this project only, a line for each burst, a
// cap on lines and on their length. It names things; `git diff` and the backups hold the detail.

use crate::paths::{STATE_DIR, events_file};
use crate::util::read_text;
use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use std::path::Path;

const LINE_CHARS: usize = 200;
const NAMES: usize = 3;
const NAMES_FILTERED: usize = 20;
// Events this close, of one kind and from one Studio, are one line.
const BURST_MS: i64 = 60_000;
// What the daemon writes for the events this shows: the rest (regenerates, applied) is most of the
// file, and skipping those lines unparsed keeps this fast.
const SHOWN: [&str; 6] = ["model", "drift", "errors", "backup", "resync", "daemon"];

pub struct Filter<'a> {
	pub project: &'a str,
	pub since_ms: i64,
	pub file: Option<&'a str>,
	pub lines: usize,
}

struct Entry {
	at: i64,
	what: String,
	who: String,
	names: Vec<String>,
	note: Option<String>,
}

fn short(path: &str) -> String {
	Path::new(path).strip_prefix(&*STATE_DIR).map_or_else(|_| path.to_string(), |p| p.display().to_string().replace('\\', "/"))
}

fn strings(value: &Value) -> Vec<String> {
	value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}

// The entries one event stands for.
fn entries(event: &Value, at: i64) -> Vec<Entry> {
	let entry = |what: String, who: &Value, names: Vec<String>, note: Option<String>| Entry {
		at,
		what,
		who: who.as_str().unwrap_or_default().to_string(),
		names,
		note,
	};
	let none = Value::Null;
	match event["ev"].as_str().unwrap_or_default() {
		"model" => {
			let name = event["file"].as_str().or(event["path"].as_str()).unwrap_or("?").to_string();
			let what = match event["action"].as_str().unwrap_or_default() {
				"saved" => "saved from Studio".to_string(),
				"load" => "loaded in Studio from its newer file".to_string(),
				_ => format!("not saved ({})", event["error"].as_str().unwrap_or("?")),
			};
			let note = event["replaced"].as_str().map(|dir| format!("old file: {}", short(dir)));
			vec![entry(what, &event["place"], vec![name], note)]
		}
		"drift" => {
			let mut found = Vec::new();
			for (field, what) in [("missing", "gone from Studio, so a full sync"), ("edited", "differs from its file")] {
				let names = strings(&event[field]);
				if !names.is_empty() {
					found.push(entry(what.to_string(), &event["who"], names, None));
				}
			}
			found
		}
		"errors" => {
			let errors = strings(&event["errors"]);
			if errors.is_empty() { Vec::new() } else { vec![entry("Studio errors".to_string(), &none, errors, None)] }
		}
		"backup" => {
			let what = format!("backed up {} Studio item(s)", event["items"]);
			vec![entry(what, &none, vec![short(event["dir"].as_str().unwrap_or_default())], None)]
		}
		"resync" => vec![entry("resync".to_string(), &none, Vec::new(), None)],
		"daemon" => vec![entry(format!("daemon restarted ({})", event["version"].as_str().unwrap_or("?")), &none, Vec::new(), None)],
		_ => Vec::new(),
	}
}

fn clock(at: i64, now: DateTime<Local>) -> String {
	let time = DateTime::<Utc>::from_timestamp_millis(at).unwrap_or_default().with_timezone(&Local);
	let format = if time.date_naive() == now.date_naive() { "%H:%M" } else { "%m-%d %H:%M" };
	time.format(format).to_string()
}

pub fn cut(line: String, chars: usize) -> String {
	match line.char_indices().nth(chars) {
		Some((at, _)) => format!("{}...", &line[..at]),
		None => line,
	}
}

// The lines for these events, oldest first.
pub fn summarize(text: &str, filter: &Filter, now: DateTime<Local>) -> Vec<String> {
	let project = format!("\"project\":{}", Value::String(filter.project.to_string()));
	let mut merged: Vec<Entry> = Vec::new();
	for line in text.lines() {
		if !line.contains(&project) && !line.contains("\"ev\":\"daemon\"") {
			continue;
		}
		let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
		if !SHOWN.contains(&event["ev"].as_str().unwrap_or_default()) {
			continue;
		}
		let at = event["t"].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map_or(0, |t| t.timestamp_millis());
		if at < filter.since_ms {
			continue;
		}
		for mut entry in entries(&event, at) {
			if let Some(file) = filter.file {
				entry.names.retain(|name| name.contains(file));
				if entry.names.is_empty() {
					continue;
				}
			}
			match merged.last_mut() {
				Some(last) if last.what == entry.what && last.who == entry.who && at - last.at <= BURST_MS => {
					for name in entry.names {
						if !last.names.contains(&name) {
							last.names.push(name);
						}
					}
					last.at = at;
					last.note = entry.note.or(last.note.take());
				}
				_ => merged.push(entry),
			}
		}
	}
	let older = merged.len().saturating_sub(filter.lines);
	let names_max = if filter.file.is_some() { NAMES_FILTERED } else { NAMES };
	let mut lines = Vec::new();
	if older > 0 {
		lines.push(format!("+{older} older: -n {}", merged.len()));
	}
	for entry in &merged[older..] {
		let who = if entry.who.is_empty() { String::new() } else { format!(" ({})", entry.who) };
		let mut line = format!("{} {}{who}", clock(entry.at, now), entry.what);
		if !entry.names.is_empty() {
			let more = entry.names.len().saturating_sub(names_max);
			line = format!("{line}: {}", entry.names[..entry.names.len() - more].join(", "));
			if more > 0 {
				line = format!("{line} +{more}");
			}
		}
		if let Some(note) = &entry.note {
			line = format!("{line}; {note}");
		}
		lines.push(if filter.file.is_some() { line } else { cut(line, LINE_CHARS) });
	}
	lines
}

// 10m, 2h, 1d, in ms.
pub fn parse_age(text: &str) -> Option<i64> {
	let digits = text.len().checked_sub(1)?;
	let count: i64 = text.get(..digits)?.parse().ok()?;
	let unit: i64 = match text.get(digits..)? {
		"m" => 60_000,
		"h" => 3_600_000,
		"d" => 86_400_000,
		_ => return None,
	};
	count.checked_mul(unit)
}

// The events to look through: the rolled-over file too, only if the current one starts too late.
pub fn events_text(since_ms: i64) -> String {
	let file = events_file();
	let text = read_text(&file).unwrap_or_default();
	let first = text.lines().next().and_then(|line| serde_json::from_str::<Value>(line).ok());
	let covers = first
		.and_then(|event| event["t"].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()))
		.is_some_and(|t| t.timestamp_millis() <= since_ms);
	if covers {
		return text;
	}
	read_text(&file.with_extension("jsonl.1")).unwrap_or_default() + &text
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn event(at: i64, fields: Value) -> String {
		let mut event = fields;
		event["t"] = json!(DateTime::<Utc>::from_timestamp_millis(at).unwrap().to_rfc3339());
		event["project"] = json!("game");
		event.to_string()
	}

	fn model(at: i64, file: &str) -> String {
		event(at, json!({ "ev": "model", "action": "saved", "file": file, "place": "1" }))
	}

	fn filter(file: Option<&str>) -> Filter<'_> {
		Filter { project: "game", since_ms: 0, file, lines: 10 }
	}

	#[test]
	fn a_burst_is_one_line_with_few_names() {
		let now = Local::now();
		let at = now.timestamp_millis();
		let text = (0..50).map(|i| model(at + i, &format!("map/{i}.rbxm"))).collect::<Vec<_>>().join("\n");
		let lines = summarize(&text, &filter(None), now);
		assert_eq!(lines.len(), 1, "{lines:?}");
		assert!(lines[0].contains("saved from Studio (1): map/0.rbxm, map/1.rbxm, map/2.rbxm +47"), "{lines:?}");
		// asking about one file shows that file
		let lines = summarize(&text, &filter(Some("map/49")), now);
		assert!(lines[0].ends_with(": map/49.rbxm"), "{lines:?}");
		assert!(summarize(&text, &filter(Some("elsewhere")), now).is_empty());
	}

	#[test]
	fn the_worst_case_stays_small() {
		let now = Local::now();
		let at = now.timestamp_millis();
		// a day of separate events, with long names, other projects' events and regenerates between
		let name = "x".repeat(400);
		let mut text = Vec::new();
		for i in 0..5000 {
			let later = at + i * 2 * BURST_MS;
			text.push(model(later, &format!("{name}{i}")));
			text.push(event(later, json!({ "ev": "regen", "version": i })));
			text.push(
				json!({ "ev": "model", "project": "other", "action": "saved", "file": "no", "t": "2026-01-01T00:00:00Z" }).to_string(),
			);
		}
		let lines = summarize(&text.join("\n"), &filter(None), now);
		assert_eq!(lines.len(), 11, "the lines, and one that says how many are older");
		assert!(lines[0].starts_with("+4990 older"), "{}", lines[0]);
		let bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
		assert!(bytes < 2400, "{bytes} bytes");
	}

	#[test]
	fn only_what_git_does_not_say() {
		let now = Local::now();
		let at = now.timestamp_millis();
		let text = [
			event(at, json!({ "ev": "regen", "version": 2 })),
			event(at, json!({ "ev": "applied", "version": 2 })),
			event(at, json!({ "ev": "wait", "ok": true })),
			event(at, json!({ "ev": "drift", "who": "copy a", "missing": ["A.b"], "edited": ["A.c"] })),
			event(at, json!({ "ev": "errors", "errors": [] })),
		]
		.join("\n");
		let lines = summarize(&text, &filter(None), now);
		assert_eq!(lines.len(), 2, "{lines:?}");
		assert!(lines[0].ends_with("gone from Studio, so a full sync (copy a): A.b"), "{lines:?}");
		assert!(lines[1].ends_with("differs from its file (copy a): A.c"), "{lines:?}");
	}

	#[test]
	fn ages() {
		assert_eq!(parse_age("10m"), Some(600_000));
		assert_eq!(parse_age("2h"), Some(7_200_000));
		assert_eq!(parse_age("1d"), Some(86_400_000));
		for bad in ["", "h", "1", "1w", "-1h"] {
			assert_eq!(parse_age(bad).filter(|age| *age >= 0), None, "{bad}");
		}
	}
}
