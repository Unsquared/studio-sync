// `studio-sync build`: a place file from a project's files, read by the rules of layout.rs. The file
// formats are Rojo's, so existing projects build as they did. This file and layout.rs are all that
// reads them, so a project format of our own would start here.
//
// `cargo test -- --ignored` checks the result against `rojo build` on every registered project.

use crate::layout::{self, Entry};
use crate::tree::luau_literal;
use crate::util::{Error, Res, read, read_json, read_text, write};
use rbx_dom_weak::types::{
	Attributes, CFrame, Color3, Content, ContentId, Enum, Font, MaterialColors, Matrix3, Ref, Tags, Variant, VariantType, Vector2, Vector3,
};
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use rbx_reflection::{DataType, PropertyDescriptor, ReflectionDatabase};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const NOTES: usize = 5;

fn database() -> Res<&'static ReflectionDatabase<'static>> {
	rbx_reflection_database::get().map_err(|e| Error(format!("Roblox's class list: {e}")))
}

fn descriptor(class: &str, property: &str) -> Res<Option<&'static PropertyDescriptor<'static>>> {
	let database = database()?;
	let mut current = class;
	while let Some(found) = database.classes.get(current) {
		if let Some(descriptor) = found.properties.get(property) {
			return Ok(Some(descriptor));
		}
		let Some(parent) = found.superclass else { break };
		current = parent;
	}
	Ok(None)
}

fn numbers<const N: usize>(value: &Value) -> Option<[f32; N]> {
	let items = value.as_array().filter(|items| items.len() == N)?;
	let mut out = [0.0; N];
	for (slot, item) in out.iter_mut().zip(items) {
		*slot = item.as_f64()? as f32;
	}
	Some(out)
}

fn from_json<T: serde::de::DeserializeOwned>(value: &Value) -> Option<T> {
	serde_json::from_value(value.clone()).ok()
}

// A property value from JSON: the explicit form ({ "Vector3": [1, 2, 3] }), or a plain value that
// the property's type gives a meaning.
fn resolve(class: &str, property: &str, value: &Value) -> Res<Variant> {
	if let Some(explicit) = from_json::<Variant>(value) {
		return Ok(explicit);
	}
	let Some(descriptor) = descriptor(class, property)? else {
		crate::bail!("{class}.{property}: not in Roblox's class list here. Give its type: {{ \"Float32\": 1 }}");
	};
	let wrong = |wanted: &str| Error(format!("{class}.{property}: {value} is no {wanted}"));
	let number = || value.as_f64();
	Ok(match &descriptor.data_type {
		DataType::Enum(name) => {
			let items = &database()?.enums.get(name).ok_or_else(|| wrong(name))?.items;
			let item = value.as_str().and_then(|text| items.get(text)).ok_or_else(|| wrong(name))?;
			Enum::from_u32(*item).into()
		}
		DataType::Value(kind) => match kind {
			VariantType::Bool => value.as_bool().ok_or_else(|| wrong("bool"))?.into(),
			VariantType::Float32 => (number().ok_or_else(|| wrong("number"))? as f32).into(),
			VariantType::Float64 => number().ok_or_else(|| wrong("number"))?.into(),
			VariantType::Int32 => (number().ok_or_else(|| wrong("number"))? as i32).into(),
			VariantType::Int64 => (number().ok_or_else(|| wrong("number"))? as i64).into(),
			VariantType::String => value.as_str().ok_or_else(|| wrong("string"))?.into(),
			VariantType::Tags => Tags::from(from_json::<Vec<String>>(value).ok_or_else(|| wrong("list of strings"))?).into(),
			VariantType::Content => Content::from(value.as_str().ok_or_else(|| wrong("string"))?).into(),
			VariantType::ContentId => ContentId::from(value.as_str().ok_or_else(|| wrong("string"))?).into(),
			VariantType::Vector2 => {
				let [x, y] = numbers(value).ok_or_else(|| wrong("list of 2 numbers"))?;
				Vector2::new(x, y).into()
			}
			VariantType::Vector3 => {
				let [x, y, z] = numbers(value).ok_or_else(|| wrong("list of 3 numbers"))?;
				Vector3::new(x, y, z).into()
			}
			VariantType::Color3 => {
				let [r, g, b] = numbers(value).ok_or_else(|| wrong("list of 3 numbers"))?;
				Color3::new(r, g, b).into()
			}
			VariantType::CFrame => {
				let [x, y, z, a, b, c, d, e, f, g, h, i] = numbers(value).ok_or_else(|| wrong("list of 12 numbers"))?;
				let rotation = Matrix3::new(Vector3::new(a, b, c), Vector3::new(d, e, f), Vector3::new(g, h, i));
				CFrame::new(Vector3::new(x, y, z), rotation).into()
			}
			VariantType::Attributes => from_json::<Attributes>(value).ok_or_else(|| wrong("set of attributes"))?.into(),
			VariantType::Font => from_json::<Font>(value).ok_or_else(|| wrong("font"))?.into(),
			VariantType::MaterialColors => from_json::<MaterialColors>(value).ok_or_else(|| wrong("set of material colors"))?.into(),
			other => crate::bail!("{class}.{property}: give its type, as {{ \"{other:?}\": ... }}"),
		},
		_ => crate::bail!("{class}.{property}: give its type, as {{ \"Float32\": 1 }}"),
	})
}

fn attribute(name: &str, value: &Value) -> Res<Variant> {
	if let Some(explicit) = from_json::<Variant>(value) {
		return Ok(explicit);
	}
	Ok(match value {
		Value::Bool(value) => (*value).into(),
		Value::Number(value) => value.as_f64().unwrap_or_default().into(),
		Value::String(value) => value.as_str().into(),
		_ => crate::bail!("attribute {name}: {value} needs its type, as {{ \"Vector3\": [1, 2, 3] }}"),
	})
}

#[derive(Default, Serialize)]
struct Localized<'a> {
	#[serde(skip_serializing_if = "Option::is_none")]
	key: Option<&'a str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	context: Option<&'a str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	example: Option<&'a str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	source: Option<&'a str>,
	values: BTreeMap<&'a str, &'a str>,
}

// A LocalizationTable's Contents, from its .csv.
fn localization(bytes: &[u8]) -> Result<String, String> {
	let mut reader = csv::Reader::from_reader(bytes);
	let headers = reader.headers().map_err(|e| e.to_string())?.clone();
	let records = reader.into_records().collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())?;
	let mut entries = Vec::new();
	for record in &records {
		let mut entry = Localized::default();
		for (header, value) in headers.iter().zip(record.iter()).filter(|(header, value)| !header.is_empty() && !value.is_empty()) {
			match header {
				"Key" => entry.key = Some(value),
				"Context" => entry.context = Some(value),
				"Example" => entry.example = Some(value),
				"Source" => entry.source = Some(value),
				locale => {
					entry.values.insert(locale, value);
				}
			}
		}
		if entry.key.is_some() || entry.source.is_some() {
			entries.push(entry);
		}
	}
	serde_json::to_string(&entries).map_err(|e| e.to_string())
}

fn toml_to_json(value: toml::Value) -> Value {
	match value {
		toml::Value::String(value) => value.into(),
		toml::Value::Integer(value) => value.into(),
		toml::Value::Float(value) => value.into(),
		toml::Value::Boolean(value) => value.into(),
		toml::Value::Datetime(value) => value.to_string().into(),
		toml::Value::Array(values) => values.into_iter().map(toml_to_json).collect(),
		toml::Value::Table(values) => values.into_iter().map(|(key, value)| (key, toml_to_json(value))).collect::<Map<_, _>>().into(),
	}
}

struct Builder<'a> {
	dir: &'a Path,
	legacy: bool,
	dom: WeakDom,
	unknown: BTreeSet<String>,
}

impl Builder<'_> {
	fn set(&mut self, id: Ref, name: &str, value: Variant) {
		if let Some(instance) = self.dom.get_by_ref_mut(id) {
			instance.properties.insert(name.into(), value);
		}
	}

	// `properties` and `attributes` from a meta file, a model or a project node. New attributes
	// replace the ones it has.
	fn apply(&mut self, id: Ref, properties: Option<&Value>, attributes: Option<&Value>, at: &str) -> Res<()> {
		let class = self.dom.get_by_ref(id).map(|instance| instance.class.to_string()).unwrap_or_default();
		let context = |error: Error| Error(format!("{at}: {}", error.0));
		for (name, value) in properties.and_then(Value::as_object).into_iter().flatten() {
			self.set(id, name, resolve(&class, name, value).map_err(context)?);
		}
		if let Some(attributes) = attributes.and_then(Value::as_object).filter(|attributes| !attributes.is_empty()) {
			let mut all = Attributes::new();
			for (name, value) in attributes {
				all.insert(name.clone(), attribute(name, value).map_err(context)?);
			}
			self.set(id, "Attributes", all.into());
		}
		Ok(())
	}

	fn instance(&mut self, class: &str, name: &str, parent: Ref) -> Res<Ref> {
		if !database()?.classes.contains_key(class) {
			self.unknown.insert(format!("{name} ({class})"));
		}
		Ok(self.dom.insert(parent, InstanceBuilder::new(class).with_name(name)))
	}

	// A .rbxm or .rbxmx: its one top-level instance, by the name of the file.
	fn model(&mut self, file: &Path, name: &str, parent: Ref) -> Res<Ref> {
		let bytes = read(&self.dir.join(file))?;
		let is_xml = file.extension().is_some_and(|extension| extension == "rbxmx");
		let decoded = if is_xml {
			let options = rbx_xml::DecodeOptions::new().property_behavior(rbx_xml::DecodePropertyBehavior::ReadUnknown);
			rbx_xml::from_reader(bytes.as_slice(), options).map_err(|e| e.to_string())
		} else {
			rbx_binary::from_reader(bytes.as_slice()).map_err(|e| e.to_string())
		};
		let mut model = decoded.map_err(|e| Error(format!("{}: {e}", file.display())))?;
		let &[top] = model.root().children() else {
			crate::bail!("{}: a model file needs one top-level instance, this has {}", file.display(), model.root().children().len());
		};
		model.transfer(top, &mut self.dom, parent);
		if let Some(instance) = self.dom.get_by_ref_mut(top) {
			instance.name = name.to_string();
		}
		Ok(top)
	}

	fn json_model(&mut self, model: &Value, name: &str, parent: Ref, at: &str) -> Res<Ref> {
		let field = |key: &str, legacy: &str| model.get(key).or_else(|| model.get(legacy));
		let class = field("className", "ClassName").and_then(Value::as_str);
		let Some(class) = class else { crate::bail!("{at}: {name} has no className") };
		let id = self.instance(class, name, parent)?;
		self.apply(id, field("properties", "Properties"), field("attributes", "Attributes"), at)?;
		for child in field("children", "Children").and_then(Value::as_array).into_iter().flatten() {
			let child_name = child.get("name").or_else(|| child.get("Name")).and_then(Value::as_str).unwrap_or_default();
			self.json_model(child, child_name, id, at)?;
		}
		Ok(id)
	}

	fn run_context(&self, stem: &str) -> Res<Option<Variant>> {
		let item = match (stem.ends_with(".server"), stem.ends_with(".client"), self.legacy) {
			(true, _, true) => "Legacy",
			(true, _, false) => "Server",
			(_, true, false) => "Client",
			_ => return Ok(None),
		};
		let items = database()?.enums.get("RunContext").map(|found| &found.items);
		Ok(items.and_then(|items| items.get(item)).map(|value| Enum::from_u32(*value).into()))
	}

	// What a file adds to the instance it is part of.
	fn file(&mut self, id: Ref, file: &Path) -> Res<()> {
		let at = file.display().to_string();
		let full = self.dir.join(file);
		let base = file.file_name().and_then(|name| name.to_str()).unwrap_or_default();
		if base.ends_with(".meta.json") {
			let meta = read_json(&full)?;
			self.apply(id, meta.get("properties"), meta.get("attributes"), &at)?;
		} else if base.ends_with(".project.json") || base.ends_with(".model.json") {
			// the layout's or json_model's business
		} else if let Some(stem) = base.strip_suffix(".luau").or_else(|| base.strip_suffix(".lua")) {
			self.set(id, "Source", read_text(&full)?.replace("\r\n", "\n").into());
			if let Some(context) = self.run_context(stem)? {
				self.set(id, "RunContext", context);
			}
		} else if base.ends_with(".json") {
			self.set(id, "Source", format!("return {}", luau_literal(&read_json(&full)?)).into());
		} else if base.ends_with(".toml") {
			let text = read_text(&full)?;
			let parsed = toml::from_str::<toml::Value>(&text).map_err(|e| Error(format!("{at}: {e}")))?;
			self.set(id, "Source", format!("return {}", luau_literal(&toml_to_json(parsed))).into());
		} else if base.ends_with(".txt") {
			self.set(id, "Value", read_text(&full)?.into());
		} else if base.ends_with(".csv") {
			self.set(id, "Contents", localization(&read(&full)?).map_err(|e| Error(format!("{at}: {e}")))?.into());
		}
		Ok(())
	}

	fn add(&mut self, entry: &Entry, parent: Ref) -> Res<()> {
		let of = |extensions: &[&str]| {
			entry.file_paths.iter().find(|file| file.extension().is_some_and(|e| extensions.iter().any(|wanted| e == *wanted)))
		};
		let json_model = entry.file_paths.iter().find(|file| file.to_string_lossy().ends_with(".model.json"));
		let id = if let Some(file) = of(&["rbxm", "rbxmx"]) {
			self.model(file, &entry.name, parent)?
		} else if let Some(file) = json_model {
			self.json_model(&read_json(&self.dir.join(file))?, &entry.name, parent, &file.display().to_string())?
		} else {
			let id = self.instance(&entry.class_name, &entry.name, parent)?;
			for file in &entry.file_paths {
				self.file(id, file)?;
			}
			id
		};
		self.apply(id, entry.properties.as_ref(), entry.attributes.as_ref(), &entry.name)?;
		for child in entry.children.iter().filter(|child| !child.in_model) {
			self.add(child, id)?;
		}
		Ok(())
	}
}

// Studio migrates the pivot of a Model that lacks this property, which moves what it should not.
fn settle_pivots(dom: &mut WeakDom) {
	const MODELS: [&str; 8] = ["Model", "Actor", "Tool", "HopperBin", "Flag", "WorldModel", "Workspace", "Status"];
	let mut left: Vec<Ref> = dom.root().children().to_vec();
	while let Some(id) = left.pop() {
		let Some(instance) = dom.get_by_ref_mut(id) else { continue };
		if MODELS.contains(&instance.class.as_str()) {
			instance.properties.entry("NeedsPivotMigration".into()).or_insert(Variant::Bool(false));
		}
		left.extend_from_slice(instance.children());
	}
}

// Builds the project (`project` is the project file's content, which may differ from the disk's) to
// `output`: .rbxl or .rbxm, or .rbxlx or .rbxmx for XML.
pub fn place(dir: &Path, project_file: &str, project: &Value, output: &Path, notes: &mut Vec<String>) -> Res<()> {
	let root = layout::read_value(dir, project_file, project)?;
	let mut builder = Builder {
		dir,
		legacy: project["emitLegacyScripts"].as_bool().unwrap_or(true),
		dom: WeakDom::new(InstanceBuilder::new("DataModel")),
		unknown: BTreeSet::new(),
	};
	let extension = output.extension().and_then(|extension| extension.to_str()).unwrap_or_default();
	let is_place = root.class_name == "DataModel";
	if extension.starts_with("rbxl") && !is_place {
		crate::bail!("{project_file}: a place file needs a tree of class DataModel; for a model, build a .rbxm");
	}
	if is_place {
		let top = builder.dom.root_ref();
		builder.apply(top, root.properties.as_ref(), root.attributes.as_ref(), "the project's root")?;
		for child in &root.children {
			builder.add(child, top)?;
		}
	} else {
		let top = builder.dom.root_ref();
		builder.add(&root, top)?;
	}
	settle_pivots(&mut builder.dom);
	let dom = &builder.dom;
	let top = dom.root().children();
	let mut bytes = Vec::new();
	match extension {
		"rbxl" | "rbxm" => rbx_binary::to_writer(&mut bytes, dom, top).map_err(|e| e.to_string()),
		"rbxlx" | "rbxmx" => rbx_xml::to_writer_default(&mut bytes, dom, top).map_err(|e| e.to_string()),
		_ => crate::bail!("{}: build a .rbxl, .rbxm, .rbxlx or .rbxmx", output.display()),
	}
	.map_err(|e| Error(format!("{}: {e}", output.display())))?;
	write(output, bytes)?;
	if !builder.unknown.is_empty() {
		let more = builder.unknown.len().saturating_sub(NOTES);
		let names: Vec<&str> = builder.unknown.iter().take(NOTES).map(String::as_str).collect();
		let others = if more > 0 { format!(" +{more}") } else { String::new() };
		notes.push(format!("not in Roblox's class list here, check the spelling: {}{others}", names.join(", ")));
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::modeldiff;
	use serde_json::json;
	use std::fs;
	use std::path::PathBuf;
	use std::process::Command;

	fn write_file(dir: &Path, rel: &str, bytes: impl AsRef<[u8]>) {
		let file = dir.join(rel);
		fs::create_dir_all(file.parent().unwrap()).unwrap();
		fs::write(file, bytes).unwrap();
	}

	fn model_bytes() -> Vec<u8> {
		let part = InstanceBuilder::new("Part").with_name("inside").with_property("Anchored", true);
		let dom =
			WeakDom::new(InstanceBuilder::new("DataModel").with_child(InstanceBuilder::new("Model").with_name("saved").with_child(part)));
		let mut bytes = Vec::new();
		rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
		bytes
	}

	// Every kind of file and value a project has.
	fn fixture(name: &str, legacy: bool) -> PathBuf {
		let dir = std::env::temp_dir().join(format!("studio-sync-build-{name}-{}", std::process::id()));
		let _ = fs::remove_dir_all(&dir);
		fs::create_dir_all(&dir).unwrap();
		fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("rokit.toml"), dir.join("rokit.toml")).unwrap();
		for (rel, text) in [
			("src/a.luau", "return 1\r\n"),
			("src/b.server.luau", "print(1)"),
			("src/b.meta.json", r#"{"properties":{"Disabled":true},"attributes":{"n":1,"s":"x","b":true,"v":{"Vector3":[1,2,3]}}}"#),
			("src/c.client.luau", "print(2)"),
			("src/d.lua", "return 3"),
			("src/mod/init.luau", "return {}"),
			("src/mod/init.meta.json", r#"{"attributes":{"on":true}}"#),
			("src/mod/child.luau", "return 6"),
			("src/conf/init.meta.json", r#"{"className":"Configuration","properties":{"Archivable":false}}"#),
			("src/conf/v.txt", "x\r\ny"),
			("src/srv/init.server.luau", "print(3)"),
			("src/data.json", r#"{"b":[1,2.5,"three",null,{"x y":true}],"a":{},"c":[]}"#),
			("src/note.txt", "hello"),
			("src/note.meta.json", r#"{"attributes":{"k":"v"}}"#),
			(
				"src/thing.model.json",
				r#"{"className":"Folder","attributes":{"t":1},"children":[{"name":"P","className":"Part","properties":{"Size":[1,2,3],"Anchored":true,"Material":"Neon","Color":[0.5,0.25,1],"Tags":["x","y"],"Transparency":0.5,"CFrame":[1,2,3,1,0,0,0,1,0,0,0,1]},"children":[{"name":"W","className":"StringValue","properties":{"Value":"hi"}}]}]}"#,
			),
			("src/loc.csv", "Key,Source,Context,Example,es,,fr\nk,hi,c,e,hola,,\n,only source,,,,,salut\n,,,,dropped,,\n"),
			("src/settings.toml", "a = 1\nb = 2.5\nc = \"x\"\nd = [1, 2]\ne = true\nwhen = 2026-01-02\n[t]\nk = \"v\"\n"),
			("src/ignored.md", "# not synced"),
			(
				"src/m.rbxmx",
				r#"<roblox version="4"><Item class="Part" referent="0"><Properties><string name="Name">M</string><bool name="Anchored">true</bool></Properties></Item></roblox>"#,
			),
			("src/side.project.json", r#"{"name":"named","tree":{"$className":"Folder","$attributes":{"a":1}}}"#),
			("src/pkg/default.project.json", r#"{"name":"lib","tree":{"$path":"src"}}"#),
			("src/pkg/src/init.luau", "return 7"),
			("src/pkg/src/k.luau", "return 8"),
			("lib/file.luau", "return 'lib'"),
		] {
			write_file(&dir, rel, text);
		}
		write_file(&dir, "src/saved.rbxm", model_bytes());
		write_file(&dir, "src/empty/.gitkeep", "");
		let project = json!({
			"name": "fx",
			"emitLegacyScripts": legacy,
			"tree": {
				"$className": "DataModel",
				"ReplicatedStorage": {
					"src": { "$path": "src" },
					"Lib": { "$path": "lib/file.luau", "$attributes": { "fromProject": true } },
					"Decl": { "$className": "Folder", "Inside": { "$path": "lib" }, "$properties": { "Archivable": false } },
					"Opt": { "$path": { "optional": "missing" } },
					"Over": { "$path": "src/saved.rbxm", "$properties": { "Archivable": false }, "Extra": { "$className": "Folder" } },
				},
				"Workspace": {
					"$properties": { "Gravity": 10, "SignalBehavior": "Deferred", "StreamingEnabled": true, "FallenPartsDestroyHeight": { "Float32": -100 } },
					"$attributes": { "w": "x" },
				},
				"Lighting": { "$properties": { "Technology": "Future", "Ambient": [0.5, 0.5, 0.5], "ClockTime": 12 } },
				"Players": { "$properties": { "CharacterAutoLoads": false, "RespawnTime": 3 } },
				"ServerScriptService": { "$className": "ServerScriptService", "srv": { "$path": "src/srv" } },
			},
		});
		write_file(&dir, "default.project.json", project.to_string());
		dir
	}

	fn build_fixture(dir: &Path) -> Vec<u8> {
		let project = read_json(&dir.join("default.project.json")).unwrap();
		let output = dir.join("ours.rbxl");
		place(dir, "default.project.json", &project, &output, &mut Vec::new()).unwrap();
		fs::read(output).unwrap()
	}

	fn find<'a>(dom: &'a WeakDom, path: &[&str]) -> &'a rbx_dom_weak::Instance {
		let mut at = dom.root();
		for name in path {
			let found = at.children().iter().filter_map(|child| dom.get_by_ref(*child)).find(|child| child.name == *name);
			at = found.unwrap_or_else(|| panic!("no {name} in {path:?}"));
		}
		at
	}

	fn property<'a>(instance: &'a rbx_dom_weak::Instance, name: &str) -> &'a Variant {
		instance.properties.get(&name.into()).unwrap_or_else(|| panic!("no {name} on {}", instance.name))
	}

	#[test]
	fn builds_every_kind_of_file() {
		let dir = fixture("kinds", true);
		let dom = rbx_binary::from_reader(build_fixture(&dir).as_slice()).unwrap();
		fs::remove_dir_all(&dir).unwrap();
		let src = ["ReplicatedStorage", "src"];
		let at = |rest: &[&str]| find(&dom, &[&src[..], rest].concat());
		assert_eq!(property(at(&["a"]), "Source"), &Variant::String("return 1\n".into()), "CRLF becomes LF");
		assert_eq!(at(&["a"]).class.as_str(), "ModuleScript");
		assert_eq!(at(&["b"]).class.as_str(), "Script");
		assert_eq!(at(&["c"]).class.as_str(), "LocalScript");
		assert_eq!(property(at(&["b"]), "Disabled"), &Variant::Bool(true));
		let Variant::Attributes(attributes) = property(at(&["b"]), "Attributes") else { panic!() };
		assert_eq!(attributes.get("n"), Some(&Variant::Float64(1.0)));
		assert_eq!(attributes.get("v"), Some(&Variant::Vector3(Vector3::new(1.0, 2.0, 3.0))));
		assert_eq!(at(&["conf"]).class.as_str(), "Configuration");
		assert_eq!(at(&["mod", "child"]).class.as_str(), "ModuleScript");
		assert_eq!(property(at(&["note"]), "Value"), &Variant::String("hello".into()));
		assert_eq!(at(&["thing", "P", "W"]).class.as_str(), "StringValue");
		assert_eq!(property(at(&["thing", "P"]), "Size"), &Variant::Vector3(Vector3::new(1.0, 2.0, 3.0)));
		let Variant::String(contents) = property(at(&["loc"]), "Contents") else { panic!() };
		assert_eq!(serde_json::from_str::<Value>(contents).unwrap().as_array().unwrap().len(), 2, "{contents}");
		assert!(matches!(property(at(&["settings"]), "Source"), Variant::String(source) if source.contains("k = \"v\"")));
		assert_eq!(at(&["m"]).class.as_str(), "Part");
		assert_eq!(at(&["saved"]).class.as_str(), "Model", "a model keeps its class, under the file's name");
		assert_eq!(at(&["saved", "inside"]).class.as_str(), "Part");
		assert_eq!(at(&["lib", "k"]).class.as_str(), "ModuleScript");
		let over = find(&dom, &["ReplicatedStorage", "Over"]);
		assert_eq!(property(over, "Archivable"), &Variant::Bool(false), "the project's properties win over the model's");
		find(&dom, &["ReplicatedStorage", "Over", "Extra"]);
		assert!(at(&[]).children().iter().all(|child| dom.get_by_ref(*child).unwrap().name != "ignored"));
		let workspace = find(&dom, &["Workspace"]);
		assert_eq!(property(workspace, "Gravity"), &Variant::Float32(10.0));
		assert!(matches!(property(find(&dom, &["Lighting"]), "Technology"), Variant::Enum(_)));
	}

	#[test]
	fn run_contexts_without_legacy_scripts() {
		let dir = fixture("modern", false);
		let dom = rbx_binary::from_reader(build_fixture(&dir).as_slice()).unwrap();
		fs::remove_dir_all(&dir).unwrap();
		let script = find(&dom, &["ReplicatedStorage", "src", "c"]);
		assert_eq!(script.class.as_str(), "Script");
		assert!(matches!(property(script, "RunContext"), Variant::Enum(_)));
	}

	#[test]
	fn bad_values_say_which() {
		assert_eq!(resolve("Part", "Anchored", &json!(true)).unwrap(), Variant::Bool(true));
		assert_eq!(resolve("Part", "Size", &json!({ "Vector3": [1, 2, 3] })).unwrap(), Variant::Vector3(Vector3::new(1.0, 2.0, 3.0)));
		for (property, value, says) in [
			("Anchored", json!("yes"), "Part.Anchored"),
			("Size", json!([1, 2]), "3 numbers"),
			("Material", json!("Cheese"), "Part.Material"),
			("NotAProperty", json!(1), "not in Roblox's class list"),
		] {
			let error = resolve("Part", property, &value).unwrap_err();
			assert!(error.0.contains(says), "{error}");
		}
	}

	#[test]
	fn a_model_file_needs_one_instance() {
		let dir = std::env::temp_dir().join(format!("studio-sync-build-two-{}", std::process::id()));
		let _ = fs::remove_dir_all(&dir);
		let dom =
			WeakDom::new(InstanceBuilder::new("DataModel").with_children([InstanceBuilder::new("Part"), InstanceBuilder::new("Part")]));
		let mut bytes = Vec::new();
		rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
		write_file(&dir, "two.rbxm", bytes);
		let project = json!({ "name": "t", "tree": { "$className": "DataModel", "Workspace": { "two": { "$path": "two.rbxm" } } } });
		let error = place(&dir, "p.project.json", &project, &dir.join("o.rbxl"), &mut Vec::new()).unwrap_err();
		fs::remove_dir_all(&dir).unwrap();
		assert!(error.0.contains("one top-level instance"), "{error}");
	}

	// Rojo's build of a project, for the checks below (needs `rojo`, through rokit in `dir`).
	fn rojo_build(dir: &Path, base: &Path, project: &Value, output: &Path) {
		let temp = base.join(format!(".studio-sync-test-{}.project.json", std::process::id()));
		fs::write(&temp, project.to_string()).unwrap();
		let done = Command::new("rojo").arg("build").arg(&temp).arg("-o").arg(output).current_dir(dir).output().unwrap();
		fs::remove_file(&temp).unwrap();
		assert!(done.status.success(), "{}: {}", dir.display(), String::from_utf8_lossy(&done.stderr));
	}

	#[test]
	#[ignore = "needs rojo"]
	fn matches_rojo_on_every_kind_of_file() {
		for legacy in [true, false] {
			let dir = fixture(if legacy { "rojo-legacy" } else { "rojo-modern" }, legacy);
			let ours = build_fixture(&dir);
			let project = read_json(&dir.join("default.project.json")).unwrap();
			let output = dir.join("rojo.rbxl");
			rojo_build(&dir, &dir, &project, &output);
			let found = modeldiff::compare(&fs::read(&output).unwrap(), &ours, Some(50)).unwrap();
			fs::remove_dir_all(&dir).unwrap();
			assert_eq!(found, ["same content; only the file's bytes differ"], "legacy {legacy}");
		}
	}

	// What real projects hold: `cargo test -- --ignored`.
	#[test]
	#[ignore = "needs rojo"]
	fn matches_rojo_on_registered_projects() {
		let registry = crate::registry::Registry::read().unwrap();
		let scratch = std::env::temp_dir().join(format!("studio-sync-build-real-{}", std::process::id()));
		fs::create_dir_all(&scratch).unwrap();
		let mut failed = Vec::new();
		for dir in registry.projects.iter().filter(|dir| dir.exists()) {
			let Ok(config) = crate::config::Config::load(dir) else { continue };
			for snapshot in [true, false] {
				let options = crate::snapshot::BuildOptions { output: scratch.join("ours.rbxl"), snapshot };
				let (project, base) = crate::snapshot::build_project(dir, &config, &options, &mut Vec::new()).unwrap();
				place(dir, &config.project, &project, &options.output, &mut Vec::new()).unwrap();
				let theirs = scratch.join("rojo.rbxl");
				rojo_build(dir, &base, &project, &theirs);
				let found = modeldiff::compare(&fs::read(&theirs).unwrap(), &fs::read(&options.output).unwrap(), Some(30)).unwrap();
				eprintln!("{} snapshot={snapshot}: {found:?}", dir.display());
				if found != ["same content; only the file's bytes differ"] {
					failed.push(format!("{} (snapshot {snapshot}):\n  {}", dir.display(), found.join("\n  ")));
				}
			}
		}
		fs::remove_dir_all(&scratch).unwrap();
		assert!(failed.is_empty(), "{}", failed.join("\n"));
	}
}
