// `studio-sync changes <model>`: what differs between two versions of an .rbxm, grouped, because git
// only says "Binary files differ" and a text dump of a map runs to megabytes. An agent pays for each
// line on every later turn, so it says what changed on how many instances and names a few of them.
//
// Only this reads model files: the sync never does, so a file Roblox's new format breaks here costs
// this one command and no sync.

use crate::changes::cut;
use crate::util::Res;
use rbx_dom_weak::types::{Ref, Variant};
use rbx_dom_weak::{Instance, WeakDom};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::process::{Command, Stdio};

const KINDS: usize = 12;
const NAMES: usize = 3;
const LINE_CHARS: usize = 200;
const VALUE_CHARS: usize = 90;
// More different values of one property than this say "changed": 500 moved parts are one line.
const VALUES: usize = 3;
// New on each save, with nothing a person changed.
const IDS: [&str; 3] = ["UniqueId", "HistoryId", "ScriptGuid"];

struct Tree<'a> {
	dom: &'a WeakDom,
	refs: BTreeMap<String, Ref>,
	paths: HashMap<Ref, String>,
}

fn tree(dom: &WeakDom) -> Tree<'_> {
	fn walk<'a>(tree: &mut Tree<'a>, parent: &'a Instance, prefix: &str) {
		let mut seen: HashMap<&str, usize> = HashMap::new();
		for child in parent.children() {
			let Some(instance) = tree.dom.get_by_ref(*child) else { continue };
			let count = seen.entry(&instance.name).or_default();
			*count += 1;
			let name = if *count > 1 { format!("{}#{count}", instance.name) } else { instance.name.clone() };
			let path = if prefix.is_empty() { name } else { format!("{prefix}.{name}") };
			tree.refs.insert(path.clone(), *child);
			tree.paths.insert(*child, path.clone());
			walk(tree, instance, &path);
		}
	}
	let mut found = Tree { dom, refs: BTreeMap::new(), paths: HashMap::new() };
	walk(&mut found, dom.root(), "");
	found
}

fn show(value: &Variant, tree: &Tree) -> String {
	match value {
		Variant::Ref(target) if target.is_none() => "nil".to_string(),
		Variant::Ref(target) => tree.paths.get(target).cloned().unwrap_or_else(|| "?".to_string()),
		Variant::String(text) => format!("{text:?}"),
		Variant::Bool(value) => value.to_string(),
		Variant::Int32(value) => value.to_string(),
		Variant::Int64(value) => value.to_string(),
		Variant::Float32(value) => value.to_string(),
		Variant::Float64(value) => value.to_string(),
		Variant::Enum(value) => value.to_u32().to_string(),
		Variant::Vector2(v) => format!("{}, {}", v.x, v.y),
		Variant::Vector3(v) => format!("{}, {}, {}", v.x, v.y, v.z),
		Variant::Color3(c) => format!("{}, {}, {}", c.r, c.g, c.b),
		Variant::Color3uint8(c) => format!("{}, {}, {}", c.r, c.g, c.b),
		other => format!("{other:?}"),
	}
}

fn from_to(from: String, to: String) -> String {
	let text = format!("{from} -> {to}");
	if text.chars().count() > VALUE_CHARS { "changed".to_string() } else { text }
}

fn tags(instance: &Instance) -> BTreeSet<&str> {
	match instance.properties.get(&"Tags".into()) {
		Some(Variant::Tags(tags)) => tags.iter().collect(),
		_ => BTreeSet::new(),
	}
}

fn attributes(instance: &Instance) -> BTreeMap<&str, &Variant> {
	match instance.properties.get(&"Attributes".into()) {
		Some(Variant::Attributes(attributes)) => attributes.iter().map(|(key, value)| (key.as_str(), value)).collect(),
		_ => BTreeMap::new(),
	}
}

#[derive(Default)]
struct Found {
	groups: BTreeMap<(String, String), Vec<String>>,
	// A property that only one file has comes from the version of Studio that saved it.
	one_side: BTreeMap<String, usize>,
}

fn compare_instance(found: &mut Found, path: &str, old: (&Instance, &Tree), new: (&Instance, &Tree)) {
	let mut add = |property: String, change: String| found.groups.entry((property, change)).or_default().push(path.to_string());
	let (old_instance, old_tree) = old;
	let (new_instance, new_tree) = new;
	if old_instance.class != new_instance.class {
		return add("class".to_string(), format!("{} -> {}", old_instance.class, new_instance.class));
	}
	let (was, is) = (tags(old_instance), tags(new_instance));
	if was != is {
		let gone = was.difference(&is).map(|tag| format!("-{tag}"));
		let came = is.difference(&was).map(|tag| format!("+{tag}"));
		add("Tags".to_string(), gone.chain(came).collect::<Vec<_>>().join(" "));
	}
	let (was, is) = (attributes(old_instance), attributes(new_instance));
	for key in was.keys().chain(is.keys()).collect::<BTreeSet<_>>() {
		let shown = |value: Option<&&Variant>, tree| value.map_or("nil".to_string(), |value| show(value, tree));
		let (from, to) = (shown(was.get(key), old_tree), shown(is.get(key), new_tree));
		if from != to {
			add(format!("@{key}"), from_to(from, to));
		}
	}
	let keys: BTreeSet<_> = old_instance.properties.keys().chain(new_instance.properties.keys()).collect();
	let mut one_side = Vec::new();
	for key in keys {
		if ["Tags", "Attributes"].contains(&key.as_str()) || IDS.contains(&key.as_str()) {
			continue;
		}
		match (old_instance.properties.get(key), new_instance.properties.get(key)) {
			(Some(from), Some(to)) => {
				let same = match (from, to) {
					(Variant::Ref(_), Variant::Ref(_)) => show(from, old_tree) == show(to, new_tree),
					_ => from == to,
				};
				if !same {
					add(key.to_string(), from_to(show(from, old_tree), show(to, new_tree)));
				}
			}
			_ => one_side.push(key.to_string()),
		}
	}
	for key in one_side {
		*found.one_side.entry(key).or_default() += 1;
	}
}

// The top ones only: what is under an added or removed instance went with it.
fn tops(names: &[&String]) -> Vec<String> {
	let all: BTreeSet<&str> = names.iter().map(|name| name.as_str()).collect();
	let is_top = |name: &&&String| name.rsplit_once('.').is_none_or(|(parent, _)| !all.contains(parent));
	names.iter().filter(is_top).map(|name| name.to_string()).collect()
}

// With `limit` (the -n of someone who wants more): up to that many lines and names, in full.
fn named(names: &[String], limit: Option<usize>) -> String {
	let max = limit.unwrap_or(NAMES);
	let tail = |name: &String, parts: usize| {
		let mut last: Vec<&str> = name.rsplit('.').take(parts).collect();
		last.reverse();
		last.join(".")
	};
	let first = &names[..names.len().min(max)];
	let leaves: BTreeSet<String> = first.iter().map(|name| tail(name, 1)).collect();
	// Three times "body" says nothing: then with its parent.
	let parts = if limit.is_some() {
		usize::MAX
	} else if leaves.len() < first.len() {
		2
	} else {
		1
	};
	let shown: Vec<String> = first.iter().map(|name| tail(name, parts)).collect();
	let more = names.len().saturating_sub(max);
	if more > 0 { format!("{} +{more}", shown.join(", ")) } else { shown.join(", ") }
}

fn read(bytes: &[u8], label: &str) -> Res<WeakDom> {
	match rbx_binary::from_reader(bytes) {
		Ok(dom) => Ok(dom),
		Err(error) => crate::bail!("{label}: can't read this model ({error}). Nothing else depends on this: the sync goes on."),
	}
}

pub fn compare(old: &[u8], new: &[u8], limit: Option<usize>) -> Res<Vec<String>> {
	let (old_dom, new_dom) = (read(old, "the old version")?, read(new, "the new version")?);
	let (old_tree, new_tree) = (tree(&old_dom), tree(&new_dom));
	let mut found = Found::default();
	for (path, new_ref) in &new_tree.refs {
		if let (Some(old_ref), Some(new_instance)) = (old_tree.refs.get(path), new_dom.get_by_ref(*new_ref))
			&& let Some(old_instance) = old_dom.get_by_ref(*old_ref)
		{
			compare_instance(&mut found, path, (old_instance, &old_tree), (new_instance, &new_tree));
		}
	}
	let mut lines = Vec::new();
	let missing_from = |from: &Tree, other: &Tree| -> Vec<String> {
		let names: Vec<&String> = from.refs.keys().filter(|path| !other.refs.contains_key(*path)).collect();
		tops(&names)
	};
	for (word, names) in [("removed", missing_from(&old_tree, &new_tree)), ("added", missing_from(&new_tree, &old_tree))] {
		if !names.is_empty() {
			lines.push(format!("{word} {}: {}", names.len(), named(&names, limit)));
		}
	}

	let mut values: BTreeMap<String, usize> = BTreeMap::new();
	for (property, _) in found.groups.keys() {
		*values.entry(property.clone()).or_default() += 1;
	}
	let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
	for ((property, change), names) in found.groups {
		let change = if values[&property] > VALUES { "changed".to_string() } else { change };
		groups.entry((property, change)).or_default().extend(names);
	}
	let mut groups: Vec<_> = groups.into_iter().collect();
	groups.sort_by_key(|group| std::cmp::Reverse(group.1.len()));
	let kinds = limit.unwrap_or(KINDS);
	let more: usize = groups.len().saturating_sub(kinds);
	for ((property, change), names) in &groups[..groups.len() - more] {
		lines.push(format!("{}x {property} {change}: {}", names.len(), named(names, limit)));
	}
	if more > 0 {
		lines.push(format!("+{more} more kinds of change: -n 100"));
	}
	let real_change = !lines.is_empty();
	if !found.one_side.is_empty() {
		let total: usize = found.one_side.values().sum();
		let mut names: Vec<(&String, &usize)> = found.one_side.iter().collect();
		names.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
		let first: Vec<&str> = names.iter().take(NAMES).map(|(name, _)| name.as_str()).collect();
		let others = if names.len() > NAMES { format!(" +{}", names.len() - NAMES) } else { String::new() };
		lines.push(format!("(hidden: {total} values only one version's file has, as a new Studio writes: {}{others})", first.join(", ")));
	}
	if !real_change && found.one_side.is_empty() {
		lines.push("same content; only the file's bytes differ".to_string());
	}
	Ok(if limit.is_some() { lines } else { lines.into_iter().map(|line| cut(line, LINE_CHARS)).collect() })
}

// `file` against git's version of it. None: neither has it, so it isn't a model here.
pub fn of_file(file: &Path, rev: Option<&str>, limit: Option<usize>) -> Res<Option<Vec<String>>> {
	let new = std::fs::read(file).ok();
	let label = rev.unwrap_or("HEAD");
	let name = file.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
	let git = Command::new("git")
		.args(["show", &format!("{label}:./{name}")])
		.current_dir(file.parent().unwrap_or(Path::new(".")))
		.stdin(Stdio::null())
		.output();
	let old = match git {
		Ok(output) if output.status.success() => Some(output.stdout),
		Ok(output) if rev.is_some() => crate::bail!("git show {label}: {}", String::from_utf8_lossy(&output.stderr).trim()),
		_ => None,
	};
	Ok(match (old, new) {
		(None, None) => None,
		(None, Some(new)) => {
			let dom = read(&new, "this file")?;
			Some(vec![format!("not in git at {label}: {} instances", tree(&dom).refs.len())])
		}
		(Some(_), None) => Some(vec![format!("deleted since {label}")]),
		(Some(old), Some(new)) if old == new => Some(vec![format!("no change since {label}")]),
		(Some(old), Some(new)) => Some(compare(&old, &new, limit)?),
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use rbx_dom_weak::InstanceBuilder;
	use rbx_dom_weak::types::{Attributes, Tags, Vector3};

	fn part(name: &str, tagged: &[&str]) -> InstanceBuilder {
		let mut tags = Tags::new();
		for tag in tagged {
			tags.push(tag);
		}
		InstanceBuilder::new("Part").with_name(name).with_property("Tags", tags)
	}

	fn encode(children: Vec<InstanceBuilder>) -> Vec<u8> {
		let model = InstanceBuilder::new("Model").with_name("markers").with_children(children);
		let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_child(model));
		let mut bytes = Vec::new();
		rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
		bytes
	}

	#[test]
	fn values_read_plainly() {
		let dom = rbx_dom_weak::WeakDom::new(InstanceBuilder::new("Folder"));
		let tree = Tree { dom: &dom, refs: BTreeMap::new(), paths: HashMap::new() };
		assert_eq!(show(&Variant::Vector3(Vector3::new(9.0, 1.5, 0.0)), &tree), "9, 1.5, 0");
	}

	#[test]
	fn the_same_edit_on_many_is_one_line() {
		let many = |tags: &[&str]| encode((0..2000).map(|i| part(&format!("cell{i}"), tags)).collect());
		let lines = compare(&many(&["Cell", "HouseSlot"]), &many(&["Cell"]), None).unwrap();
		assert_eq!(lines.len(), 1, "{lines:?}");
		assert!(lines[0].starts_with("2000x Tags -HouseSlot: cell0, cell1, cell10 +1997"), "{lines:?}");
		let lines = compare(&many(&["Cell", "HouseSlot"]), &many(&["Cell"]), Some(2)).unwrap();
		assert!(lines[0].contains("markers.cell0, markers.cell1 +1998"), "{lines:?}");
	}

	#[test]
	fn many_values_of_a_property_are_one_line() {
		let at = |shift: f32| {
			(0..500).map(|i| part(&format!("p{i}"), &[]).with_property("Size", Vector3::new(i as f32 + shift, 1.0, 1.0))).collect()
		};
		let lines = compare(&encode(at(0.0)), &encode(at(5.0)), None).unwrap();
		assert_eq!(lines.len(), 1, "{lines:?}");
		assert!(lines[0].starts_with("500x Size changed"), "{lines:?}");
	}

	#[test]
	fn the_worst_case_stays_small() {
		// each instance with its own kind of change, and long names
		let long = "x".repeat(300);
		let edit = |value: f64| {
			(0..400)
				.map(|i| {
					let mut attributes = Attributes::new();
					attributes.insert(format!("{long}{i}"), Variant::Float64(value));
					part(&format!("{long}{i}"), &[]).with_property("Attributes", attributes)
				})
				.collect()
		};
		let lines = compare(&encode(edit(1.0)), &encode(edit(2.0)), None).unwrap();
		assert_eq!(lines.len(), KINDS + 1, "{lines:?}");
		let bytes: usize = lines.iter().map(|line| line.len() + 1).sum();
		assert!(bytes < 3000, "{bytes} bytes");
	}

	#[test]
	fn added_removed_and_refs() {
		let child = |name: &str| part(name, &[]).with_child(part("under", &[]));
		let old = encode(vec![child("a"), child("b")]);
		let new = encode(vec![child("b"), child("c")]);
		let lines = compare(&old, &new, None).unwrap();
		assert_eq!(lines, ["removed 1: a", "added 1: c"]);
		let tagged = |tag: &[&str]| encode(["a", "b"].map(|name| part(name, &[]).with_child(part("under", tag))).into());
		let lines = compare(&tagged(&[]), &tagged(&["New"]), None).unwrap();
		assert_eq!(lines, ["2x Tags +New: a.under, b.under"]);
		assert_eq!(compare(&old, &old, None).unwrap(), ["same content; only the file's bytes differ"]);

		// a reference is the same if it points at the same place, whatever its id in the file
		let linked = |target: &str| {
			let first = part("first", &[]);
			let second = part("second", &[]);
			let aim = if target == "first" { first.referent() } else { second.referent() };
			let model = InstanceBuilder::new("Model").with_name("m").with_property("PrimaryPart", aim).with_children([first, second]);
			let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_child(model));
			let mut bytes = Vec::new();
			rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
			bytes
		};
		assert_eq!(compare(&linked("first"), &linked("first"), None).unwrap(), ["same content; only the file's bytes differ"]);
		assert_eq!(compare(&linked("first"), &linked("second"), None).unwrap(), ["1x PrimaryPart m.first -> m.second: m"]);
	}

	#[test]
	fn what_only_one_file_has_is_hidden_but_counted() {
		let old = encode(vec![part("a", &[])]);
		let new = encode(vec![part("a", &[]).with_property("Anchored", true)]);
		let lines = compare(&old, &new, None).unwrap();
		assert_eq!(lines.len(), 1, "{lines:?}");
		assert!(lines[0].starts_with("(hidden: 1 values") && lines[0].contains("Anchored"), "{lines:?}");
	}

	#[test]
	fn a_bad_file_is_an_error() {
		let error = compare(b"not a model", &encode(vec![]), None).unwrap_err();
		assert!(error.to_string().contains("can't read"), "{error}");
	}
}
