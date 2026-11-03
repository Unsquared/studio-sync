// One registered project: its config, the node tree generated from its files (tree.rs),
// the delta log the plugin reads, and the Studios whose sockets are open to it.
//
// A worker thread owns the project's file watcher. It lets a burst of events settle,
// then regenerates. A project builds its tree only once something uses it (a Studio
// connects, the CLI asks), and drops it after IDLE_AFTER with no Studio and no use.

use crate::config::Config;
use crate::log;
use crate::paths::staging_root;
use crate::tree::{self, Generated, Node, Nodes, PACKAGE_DIRS, Place, Reuse};
use crate::util::sha;
use crate::watch::{Change, Watch};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, oneshot};

const LOG_KEEP: usize = 300;
// a burst of writes (a branch switch, a formatter) settles before a regenerate
const QUIET: Duration = Duration::from_millis(50);
const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);
const IDLE_CHECK: Duration = Duration::from_secs(60);
const READY_TIMEOUT: Duration = Duration::from_secs(120);

// A Studio, connected while its socket is open.
pub struct Client {
	pub id: String,
	// which socket: a reconnect with the same id replaces the old one
	pub conn: u64,
	pub place_id: u64,
	pub session: Option<String>,
	// the version it has applied
	pub version: u64,
	pub plugin: String,
	pub errors: Vec<String>,
}

impl Client {
	pub fn describe(&self) -> String {
		match &self.session {
			Some(session) => format!("copy {session}"),
			None => format!("place {}", self.place_id),
		}
	}

	pub fn view(&self) -> Value {
		json!({
			"placeId": self.place_id,
			"session": self.session,
			"version": self.version,
			"plugin": self.plugin,
			"errors": self.errors,
		})
	}
}

#[derive(Clone, Serialize)]
pub struct Command {
	#[serde(rename = "type")]
	pub kind: &'static str,
	pub id: String,
	#[serde(skip)]
	pub client: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub path: Option<Vec<String>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub file: Option<String>,
}

struct Delta {
	version: u64,
	upserts: Nodes,
	removes: BTreeMap<String, Vec<String>>,
}

#[derive(Serialize)]
pub struct Info {
	name: String,
	epoch: String,
	version: u64,
	roots: Vec<Vec<String>>,
	writeback: bool,
}

#[derive(Serialize)]
pub struct Tree {
	#[serde(flatten)]
	info: Info,
	nodes: Vec<Arc<Node>>,
}

#[derive(Serialize)]
pub struct Changes {
	#[serde(flatten)]
	info: Info,
	full: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	upserts: Option<Vec<Arc<Node>>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	removes: Option<Vec<Vec<String>>>,
}

// What a client's socket was last told.
pub struct Rung {
	epoch: String,
	version: u64,
}

impl Rung {
	// The client's own state when its socket opens: what it fetched before.
	pub fn new(epoch: &str, version: u64) -> Rung {
		Rung { epoch: epoch.to_string(), version }
	}
}

// Studio's messages on its socket.
#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientMessage {
	Applied {
		version: u64,
	},
	Report {
		#[serde(default)]
		errors: Vec<String>,
	},
}

#[derive(Default)]
pub struct State {
	pub config: Config,
	// read from disk when a model save first needs it
	pub agreed: Option<crate::writeback::Agreed>,
	pub epoch: String,
	pub version: u64,
	log_base: u64,
	changes: VecDeque<Delta>,
	pub nodes: Nodes,
	pub places: BTreeMap<String, Place>,
	// where files made since the last regenerate live (the sourcemap doesn't know them yet)
	pub pending_places: BTreeMap<String, Place>,
	pub roots: Vec<Vec<String>>,
	pub read_only: Vec<Vec<String>>,
	pub error: Option<String>,
	pub clients: BTreeMap<String, Client>,
	pub commands: Vec<Command>,
	active: bool,
	last_used: Option<Instant>,
}

impl State {
	pub fn active(&self) -> bool {
		self.active
	}

	fn activate(&mut self) {
		self.active = true;
		self.epoch = format!("{:x}", SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis());
		self.error = None;
		self.last_used = Some(Instant::now());
	}

	fn info(&self, name: &str) -> Info {
		Info {
			name: name.to_string(),
			epoch: self.epoch.clone(),
			version: self.version,
			roots: self.roots.clone(),
			writeback: self.config.writeback,
		}
	}

	fn covers_packages(&self) -> bool {
		self.roots.iter().any(|r| r.last().is_some_and(|n| PACKAGE_DIRS.contains(&n.as_str())))
	}

	fn apply(&mut self, generated: Generated, name: &str, started: Instant) {
		let Generated { nodes, places, roots, read_only } = generated;
		let upserts: Nodes = nodes
			.iter()
			.filter(|(k, n)| self.nodes.get(*k).is_none_or(|old| old.hash != n.hash))
			.map(|(k, n)| (k.clone(), n.clone()))
			.collect();
		let removes: BTreeMap<String, Vec<String>> =
			self.nodes.iter().filter(|(k, _)| !nodes.contains_key(*k)).map(|(k, n)| (k.clone(), n.path.clone())).collect();
		let roots_changed = roots != self.roots || read_only != self.read_only;
		self.pending_places.retain(|k, _| !places.contains_key(k));
		self.nodes = nodes;
		self.places = places;
		self.roots = roots;
		self.read_only = read_only;
		self.error = None;
		if upserts.is_empty() && removes.is_empty() && !roots_changed && self.version > 0 {
			return;
		}
		self.version += 1;
		let (added, removed) = (upserts.len(), removes.len());
		if self.version == 1 || roots_changed {
			self.changes.clear();
			self.log_base = self.version;
		} else {
			self.changes.push_back(Delta { version: self.version, upserts, removes });
			if self.changes.len() > LOG_KEEP {
				self.log_base = self.changes.pop_front().map_or(self.version, |d| d.version);
			}
		}
		log!("[{name}] v{}: +{added} -{removed} ({:.2}s)", self.version, started.elapsed().as_secs_f64());
	}

	fn take_commands(&mut self, client: &str) -> Vec<Command> {
		let (mine, rest) = self.commands.drain(..).partition(|c| c.client == client);
		self.commands = rest;
		mine
	}

	// Every change after `since`, merged: the last word on each node wins. A full sync when the
	// epoch changed (a restart, an idle drop) or `since` is older than the log.
	fn delta_since(&self, name: &str, epoch: &str, since: u64) -> Changes {
		if epoch != self.epoch || since > self.version || since < self.log_base {
			return Changes { info: self.info(name), full: true, upserts: None, removes: None };
		}
		let mut upserts = Nodes::new();
		let mut removes = BTreeMap::new();
		for delta in self.changes.iter().filter(|d| d.version > since) {
			for (k, path) in &delta.removes {
				upserts.remove(k);
				removes.insert(k.clone(), path.clone());
			}
			for (k, node) in &delta.upserts {
				removes.remove(k);
				upserts.insert(k.clone(), node.clone());
			}
		}
		Changes {
			info: self.info(name),
			full: false,
			upserts: Some(upserts.into_values().collect()),
			removes: Some(removes.into_values().collect()),
		}
	}
}

enum Msg {
	Event(notify::Result<notify::Event>),
	Regenerate(Option<oneshot::Sender<()>>),
	Stop,
}

pub struct Project {
	pub dir: PathBuf,
	pub name: String,
	pub staging: PathBuf,
	state: Mutex<State>,
	changed: Notify,
	worker: Sender<Msg>,
}

impl Project {
	pub fn start(dir: PathBuf) -> Arc<Project> {
		let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
		let mut state = State::default();
		match Config::load(&dir) {
			Ok(config) => state.config = config,
			Err(e) => state.error = Some(e.0),
		}
		let (tx, rx) = mpsc::channel();
		let events = tx.clone();
		let watch = Watch::new(&dir, move |event| {
			let _ = events.send(Msg::Event(event));
		});
		let project = Arc::new(Project {
			staging: staging_root().join(&sha(dir.to_string_lossy().as_bytes())[..10]),
			dir,
			name,
			state: Mutex::new(state),
			changed: Notify::new(),
			worker: tx,
		});
		let watch = match watch {
			Ok(watch) => Some(watch),
			Err(e) => {
				log!("[{}] {e}", project.name);
				project.state().error = Some(e.0);
				None
			}
		};
		let worker = project.clone();
		thread::Builder::new()
			.name(format!("project {}", project.name))
			.spawn(move || worker.work(rx, watch))
			.expect("spawn a project thread");
		project
	}

	pub fn state(&self) -> MutexGuard<'_, State> {
		self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
	}

	pub fn wake(&self) {
		self.changed.notify_waiters();
	}

	// Removed from the registry: its sockets close, so their Studios look again.
	pub fn stop(&self) {
		let _ = self.worker.send(Msg::Stop);
		self.state().clients.clear();
		self.wake();
	}

	// Regenerates soon, without waiting for it.
	fn request_regenerate(&self) {
		let _ = self.worker.send(Msg::Regenerate(None));
	}

	// Regenerates now and returns when it is done.
	pub async fn regenerate_now(&self) {
		let (tx, rx) = oneshot::channel();
		if self.worker.send(Msg::Regenerate(Some(tx))).is_ok() {
			let _ = rx.await;
		}
	}

	// -- the worker -----------------------------------------------------------

	fn work(self: Arc<Self>, rx: Receiver<Msg>, mut watch: Option<Watch>) {
		self.update_watch(&mut watch);
		loop {
			let active = self.state().active;
			let first = if active {
				match rx.recv_timeout(IDLE_CHECK) {
					Ok(msg) => msg,
					Err(RecvTimeoutError::Timeout) => {
						self.sleep_if_idle();
						continue;
					}
					Err(RecvTimeoutError::Disconnected) => return,
				}
			} else {
				match rx.recv() {
					Ok(msg) => msg,
					Err(_) => return,
				}
			};
			let mut batch = Batch { changed: Some(BTreeSet::new()), ..Batch::default() };
			batch.add(first, watch.as_ref(), active);
			while batch.settling() {
				match rx.recv_timeout(QUIET) {
					Ok(msg) => batch.add(msg, watch.as_ref(), active),
					Err(RecvTimeoutError::Timeout) => break,
					Err(RecvTimeoutError::Disconnected) => return,
				}
			}
			while let Ok(msg) = rx.try_recv() {
				batch.add(msg, watch.as_ref(), active);
			}
			if batch.stop {
				return;
			}
			let config_ok = batch.change != Change::Config || self.reload_config();
			if config_ok && (batch.forced || (active && batch.change != Change::None)) {
				// a forced regenerate (`wait`) or a config change reads every file again
				let incremental = !batch.forced && batch.change == Change::Tree;
				self.regenerate(batch.changed.filter(|_| incremental));
			}
			if batch.forced || batch.change != Change::None {
				self.update_watch(&mut watch);
			}
			for reply in batch.replies {
				let _ = reply.send(());
			}
		}
	}

	fn update_watch(&self, watch: &mut Option<Watch>) {
		if let Some(watch) = watch {
			let st = self.state();
			watch.update(&st.config, st.covers_packages());
		}
	}

	fn reload_config(&self) -> bool {
		let loaded = Config::load(&self.dir);
		let mut st = self.state();
		match loaded {
			Ok(config) => {
				st.config = config;
				true
			}
			Err(e) => {
				log!("[{}] {e}", self.name);
				st.error = Some(e.0);
				drop(st);
				self.wake();
				false
			}
		}
	}

	// Regenerates from the files; with `changed`, it reads only those and reuses the other nodes.
	fn regenerate(&self, changed: Option<BTreeSet<PathBuf>>) {
		let started = Instant::now();
		let (config, reuse) = {
			let mut st = self.state();
			if !st.active {
				st.activate();
			}
			// after a failed regenerate the last tree is older than the changes it missed
			let reuse = changed.filter(|_| st.version > 0 && st.error.is_none()).map(|changed| Reuse {
				nodes: st.nodes.clone(),
				places: st.places.clone(),
				changed,
			});
			(st.config.clone(), reuse)
		};
		let generated = tree::generate(&self.dir, &config, reuse);
		let mut st = self.state();
		match generated {
			Ok(generated) => st.apply(generated, &self.name, started),
			Err(e) => {
				log!("[{}] {e}", self.name);
				st.error = Some(e.0);
			}
		}
		drop(st);
		self.wake();
	}

	fn sleep_if_idle(&self) {
		let mut st = self.state();
		let unused = st.last_used.is_none_or(|t| t.elapsed() >= IDLE_AFTER);
		if st.active && unused && st.clients.is_empty() && st.commands.is_empty() {
			*st = State { config: std::mem::take(&mut st.config), ..State::default() };
			log!("[{}] idle: the tree is dropped until a Studio or the CLI uses it", self.name);
		}
	}

	// -- requests -------------------------------------------------------------

	// Waits for the tree, building it first if the project is idle.
	pub async fn ready(&self) -> Result<(), String> {
		loop {
			let notified = self.changed.notified();
			tokio::pin!(notified);
			notified.as_mut().enable();
			{
				let mut st = self.state();
				st.last_used = Some(Instant::now());
				if st.version > 0 {
					return Ok(());
				}
				if !st.active {
					st.activate();
					self.request_regenerate();
				} else if let Some(error) = &st.error {
					return Err(error.clone());
				}
			}
			if tokio::time::timeout(READY_TIMEOUT, notified).await.is_err() {
				return Err("project not generated yet".into());
			}
		}
	}

	pub fn info(&self) -> Info {
		self.state().info(&self.name)
	}

	pub fn tree(&self) -> Tree {
		let st = self.state();
		Tree { info: st.info(&self.name), nodes: st.nodes.values().cloned().collect() }
	}

	pub fn changes(&self, epoch: &str, since: u64) -> Changes {
		self.state().delta_since(&self.name, epoch, since)
	}

	// -- sockets ----------------------------------------------------------------

	// A Studio's socket opened; it has applied `version`. Returns the connection's number.
	pub fn connect(&self, id: &str, place_id: u64, session: Option<&str>, plugin: &str, version: u64) -> u64 {
		let mut st = self.state();
		st.last_used = Some(Instant::now());
		let conn = st.clients.get(id).map_or(1, |c| c.conn + 1);
		let client = Client {
			id: id.to_string(),
			conn,
			place_id,
			session: session.map(str::to_string),
			version,
			plugin: plugin.to_string(),
			errors: Vec::new(),
		};
		log!("[{}] Studio connected ({}, plugin {plugin})", self.name, client.describe());
		st.clients.insert(id.to_string(), client);
		drop(st);
		self.wake();
		conn
	}

	pub fn disconnect(&self, id: &str, conn: u64) {
		let mut st = self.state();
		if st.clients.get(id).is_some_and(|c| c.conn == conn) {
			let client = st.clients.remove(id).expect("just found");
			st.commands.retain(|c| c.client != id);
			st.last_used = Some(Instant::now());
			log!("[{}] Studio disconnected ({})", self.name, client.describe());
		}
		drop(st);
		self.wake();
	}

	// What the socket of (id, conn) should be sent now: the version when it moved on from
	// `rung`, and the commands for it. None when that socket is no longer the client's.
	pub fn outgoing(&self, id: &str, conn: u64, rung: &mut Rung) -> Option<Vec<String>> {
		let mut st = self.state();
		if st.clients.get(id).is_none_or(|c| c.conn != conn) {
			return None;
		}
		let mut out = Vec::new();
		if st.version > 0 && (rung.epoch != st.epoch || rung.version != st.version) {
			*rung = Rung { epoch: st.epoch.clone(), version: st.version };
			out.push(json!({ "type": "version", "epoch": st.epoch, "version": st.version }).to_string());
		}
		for command in st.take_commands(id) {
			out.push(serde_json::to_string(&command).unwrap_or_default());
		}
		Some(out)
	}

	pub fn receive(&self, id: &str, text: &str) {
		let message = match serde_json::from_str::<ClientMessage>(text) {
			Ok(message) => message,
			Err(e) => return log!("[{}] bad message from Studio: {e}", self.name),
		};
		let mut st = self.state();
		let Some(client) = st.clients.get_mut(id) else { return };
		match message {
			ClientMessage::Applied { version } => client.version = version,
			ClientMessage::Report { errors } => {
				for error in &errors {
					log!("[{}] Studio: {error}", self.name);
				}
				client.errors = errors;
			}
		}
		drop(st);
		self.wake();
	}

	pub fn changed(&self) -> &Notify {
		&self.changed
	}

	// Regenerates now, then waits until every live Studio has applied the current version.
	pub async fn wait_applied(&self, timeout: Duration) -> Value {
		self.regenerate_now().await;
		let deadline = tokio::time::Instant::now() + timeout;
		loop {
			let notified = self.changed.notified();
			tokio::pin!(notified);
			notified.as_mut().enable();
			{
				let mut st = self.state();
				st.last_used = Some(Instant::now());
				let clients: Vec<Value> = st.clients.values().map(Client::view).collect();
				let behind = st.clients.values().any(|c| c.version < st.version);
				let reply = |ok: bool, reason: Option<&str>| json!({ "ok": ok, "reason": reason, "clients": clients, "version": st.version, "error": st.error, "project": self.name });
				if !clients.is_empty() && !behind {
					return reply(true, None);
				}
				if tokio::time::Instant::now() >= deadline {
					return reply(false, Some(if clients.is_empty() { "no-studio" } else { "timeout" }));
				}
			}
			let _ = tokio::time::timeout_at(deadline, notified).await;
		}
	}
}

#[derive(Default)]
struct Batch {
	change: Change,
	// the paths the events named, or None once one needs a full regenerate
	changed: Option<BTreeSet<PathBuf>>,
	forced: bool,
	stop: bool,
	replies: Vec<oneshot::Sender<()>>,
}

impl Batch {
	fn add(&mut self, msg: Msg, watch: Option<&Watch>, active: bool) {
		match msg {
			Msg::Event(Ok(event)) => {
				let Some(watch) = watch else { return };
				let change = watch.classify(&event);
				// an idle project has no tree to keep current, only its config
				if change == Change::Config || (active && change == Change::Tree) {
					self.change = self.change.max(change);
				}
				if change == Change::Tree {
					match (watch.touched(&event), &mut self.changed) {
						(Some(paths), Some(changed)) => changed.extend(paths),
						_ => self.changed = None,
					}
				}
			}
			Msg::Event(Err(e)) => {
				log!("watch: {e}");
				self.changed = None;
				if active {
					self.change = self.change.max(Change::Tree);
				}
			}
			Msg::Regenerate(reply) => {
				self.forced = true;
				self.replies.extend(reply);
			}
			Msg::Stop => self.stop = true,
		}
	}

	fn settling(&self) -> bool {
		self.change != Change::None && !self.forced && !self.stop
	}
}
