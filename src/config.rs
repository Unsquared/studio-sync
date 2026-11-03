// studio-sync.json, with studio-sync.local.json's keys over it (docs/config.md).

use crate::util::{Error, Res, read_json};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::path::Path;

pub const CONFIG_FILE: &str = "studio-sync.json";
pub const LOCAL_CONFIG_FILE: &str = "studio-sync.local.json";

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
	#[serde(default, deserialize_with = "place_ids")]
	pub place_ids: Vec<u64>,
	#[serde(default = "default_project")]
	pub project: String,
	#[serde(default)]
	pub roots: Option<Vec<Vec<String>>>,
	#[serde(default)]
	pub read_only: Option<Vec<Vec<String>>>,
	#[serde(default = "yes")]
	pub writeback: bool,
	#[serde(default)]
	pub content: Option<Content>,
	#[serde(default)]
	pub snapshot: Option<Snapshot>,
	#[serde(default)]
	pub copy: Option<Copy>,
}

#[derive(Clone, Deserialize)]
pub struct Content {
	pub dir: String,
	#[serde(default)]
	pub sources: Vec<String>,
}

#[derive(Clone, Default, Deserialize)]
pub struct Snapshot {
	#[serde(default)]
	pub dir: Option<String>,
	#[serde(default)]
	pub ignore: Vec<Vec<String>>,
}

#[derive(Clone, Deserialize)]
pub struct Copy {
	#[serde(default)]
	pub command: Vec<String>,
	#[serde(default)]
	pub output: String,
}

fn default_project() -> String {
	"default.project.json".into()
}

fn yes() -> bool {
	true
}

// Numbers, or numbers written as strings.
fn place_ids<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u64>, D::Error> {
	let raw = Option::<Vec<Value>>::deserialize(d)?.unwrap_or_default();
	Ok(raw.iter().filter_map(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok())).collect())
}

impl Default for Config {
	fn default() -> Self {
		serde_json::from_value(Value::Object(Default::default())).expect("every field has a default")
	}
}

impl Config {
	pub fn load(dir: &Path) -> Res<Config> {
		let Value::Object(mut merged) = read_json(&dir.join(CONFIG_FILE))? else {
			return Err(Error(format!("{CONFIG_FILE} is not a JSON object")));
		};
		let local = dir.join(LOCAL_CONFIG_FILE);
		if local.exists()
			&& let Value::Object(overrides) = read_json(&local)?
		{
			merged.extend(overrides);
		}
		serde_json::from_value(Value::Object(merged)).map_err(|e| Error(format!("{CONFIG_FILE}: {e}")))
	}

	pub fn snapshot_dir(&self) -> &str {
		self.snapshot.as_ref().and_then(|s| s.dir.as_deref()).unwrap_or("place")
	}
}
