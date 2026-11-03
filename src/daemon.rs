// The studio-sync daemon: one HTTP server on localhost for every registered
// project. Studio finds its project by place id; the CLI finds it by directory.
// Protocol: docs/protocol.md.
//
// Each Studio holds a WebSocket open (/socket): the daemon pushes new versions and
// commands down it, and a Studio is connected exactly while it is open. Trees,
// deltas and uploads stay on plain HTTP. Disk work runs on the blocking pool.
// projects.json is watched, not polled.

use crate::config::CONFIG_FILE;
use crate::log;
use crate::paths::{PORT, STATE_DIR, VERSION, projects_file};
use crate::project::{Client, Command, Project, Rung};
use crate::registry::{Registry, Session};
use crate::snapshot::{self, Uploads};
use crate::util::{Error, files_under, hex, mkdirs, random_hex, read, read_text, remove, slashes, write};
use crate::writeback::{BackupItem, Writer, save_backup};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use notify::{RecursiveMode, Watcher};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

const MAX_BODY: usize = 64 * 1024 * 1024;
const PULL_TIMEOUT: Duration = Duration::from_secs(60);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(600);

// -- replies ------------------------------------------------------------------

struct Reply {
	status: u16,
	body: Vec<u8>,
	json: bool,
}

impl Reply {
	fn text(status: u16, body: impl Into<String>) -> Self {
		Reply { status, body: body.into().into_bytes(), json: false }
	}

	fn ok() -> Self {
		Reply::text(200, "ok")
	}

	fn json(status: u16, value: &impl Serialize) -> Self {
		Reply { status, body: serde_json::to_vec(value).unwrap_or_default(), json: true }
	}

	fn response(self) -> Response<Full<Bytes>> {
		let content_type = if self.json { "application/json" } else { "text/plain; charset=utf-8" };
		let mut response = Response::new(Full::new(Bytes::from(self.body)));
		*response.status_mut() = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
		response.headers_mut().insert("content-type", hyper::header::HeaderValue::from_static(content_type));
		response
	}
}

struct Reject {
	status: u16,
	message: String,
}

fn reject(status: u16, message: impl Into<String>) -> Reject {
	Reject { status, message: message.into() }
}

impl From<Error> for Reject {
	fn from(e: Error) -> Self {
		reject(500, e.0)
	}
}

impl From<serde_json::Error> for Reject {
	fn from(e: serde_json::Error) -> Self {
		reject(400, e.to_string())
	}
}

type Handled = Result<Reply, Reject>;

// -- query strings --------------------------------------------------------------

struct Query(Vec<(String, String)>);

impl Query {
	fn parse(raw: Option<&str>) -> Query {
		Query(
			raw.unwrap_or_default()
				.split('&')
				.filter(|pair| !pair.is_empty())
				.map(|pair| {
					let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
					(decode(k), decode(v))
				})
				.collect(),
		)
	}

	fn get(&self, key: &str) -> Option<&str> {
		self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
	}

	// A present, non-empty value.
	fn some(&self, key: &str) -> Option<&str> {
		self.get(key).filter(|v| !v.is_empty())
	}

	fn number(&self, key: &str) -> Option<u64> {
		self.get(key)?.parse::<f64>().ok().filter(|n| *n >= 0.0).map(|n| n as u64)
	}

	fn path(&self, key: &str) -> Result<Vec<String>, Reject> {
		Ok(serde_json::from_str(self.get(key).unwrap_or("[]"))?)
	}
}

// application/x-www-form-urlencoded, as URLSearchParams reads it
fn decode(text: &str) -> String {
	let bytes = text.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		match bytes[i] {
			b'+' => out.push(b' '),
			b'%' if i + 2 < bytes.len() => match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("x"), 16) {
				Ok(b) => {
					out.push(b);
					i += 2;
				}
				Err(_) => out.push(b'%'),
			},
			b => out.push(b),
		}
		i += 1;
	}
	String::from_utf8_lossy(&out).into_owned()
}

// -- the daemon -------------------------------------------------------------------

type CommandResult = Value;

#[derive(Default)]
struct Daemon {
	projects: RwLock<Vec<Arc<Project>>>,
	claims: Mutex<BTreeMap<String, PathBuf>>,
	sessions: Mutex<BTreeMap<String, Session>>,
	// a command's id -> (the client it went to, the CLI request waiting for its result)
	waiters: Mutex<HashMap<String, (String, oneshot::Sender<CommandResult>)>>,
	pull_parts: Mutex<HashMap<String, String>>,
	uploads: Mutex<Uploads>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Daemon {
	fn projects(&self) -> Vec<Arc<Project>> {
		self.projects.read().unwrap_or_else(|p| p.into_inner()).clone()
	}

	fn refresh_registry(&self) {
		let registry = match Registry::read() {
			Ok(registry) => registry,
			Err(e) => return log!("registry: {e}"),
		};
		{
			let mut projects = self.projects.write().unwrap_or_else(|p| p.into_inner());
			projects.retain(|project| {
				let keep = registry.projects.contains(&project.dir);
				if !keep {
					project.stop();
					log!("removed {}", project.dir.display());
				}
				keep
			});
			for dir in &registry.projects {
				if !projects.iter().any(|p| &p.dir == dir) && dir.join(CONFIG_FILE).exists() {
					projects.push(Project::start(dir.clone()));
					log!("added {}", dir.display());
				}
			}
		}
		*lock(&self.claims) = registry.claims;
		*lock(&self.sessions) = registry.sessions;
	}

	fn by_place(&self, raw: Option<&str>, session: Option<&str>) -> Result<Arc<Project>, Reject> {
		let projects = self.projects();
		if let Some(session) = session {
			let dir = lock(&self.sessions).get(session).map(|s| s.dir.clone());
			return dir
				.and_then(|dir| projects.into_iter().find(|p| p.dir == dir))
				.ok_or_else(|| reject(404, format!("no project for copy {session} (studio-sync copies)")));
		}
		let place_id = raw.and_then(|r| r.parse::<u64>().ok()).filter(|id| *id != 0);
		let Some(place_id) = place_id else {
			return Err(reject(404, "no placeId (publish the place: a local file has place id 0)"));
		};
		let lists = |p: &Arc<Project>| p.state().config.place_ids.contains(&place_id);
		if let Some(claimed) = lock(&self.claims).get(&place_id.to_string())
			&& let Some(project) = projects.iter().find(|p| &p.dir == claimed && lists(p))
		{
			return Ok(project.clone());
		}
		let owners: Vec<_> = projects.into_iter().filter(lists).collect();
		match owners.as_slice() {
			[one] => Ok(one.clone()),
			[] => Err(reject(404, format!("no project for place {place_id}"))),
			many => Err(reject(
				409,
				format!(
					"place {place_id} is listed by {}: run `studio-sync claim` in the one to sync",
					many.iter().map(|p| p.dir.display().to_string()).collect::<Vec<_>>().join(" and ")
				),
			)),
		}
	}

	fn by_dir(&self, raw: Option<&str>) -> Result<Arc<Project>, Reject> {
		let not_found = || reject(404, format!("no registered project contains {}", raw.unwrap_or("undefined")));
		let dir = std::path::absolute(raw.ok_or_else(not_found)?).map_err(|_| not_found())?;
		let projects = self.projects();
		dir.ancestors().find_map(|d| projects.iter().find(|p| p.dir == d).cloned()).ok_or_else(not_found)
	}

	fn status(&self) -> Value {
		let claims = lock(&self.claims).clone();
		let projects: Vec<Value> = self
			.projects()
			.iter()
			.map(|p| {
				let st = p.state();
				let place_ids = st.config.place_ids.clone();
				json!({
					"dir": p.dir,
					"name": p.name,
					"placeIds": place_ids,
					"claimed": place_ids.iter().filter(|id| claims.get(&id.to_string()) == Some(&p.dir)).collect::<Vec<_>>(),
					"active": st.active(),
					"version": st.version,
					"nodes": st.nodes.len(),
					"roots": st.roots.iter().map(|r| r.join(".")).collect::<Vec<_>>(),
					"readOnly": st.read_only.iter().map(|r| r.join(".")).collect::<Vec<_>>(),
					"writeback": st.config.writeback,
					"error": st.error,
					"clients": st.clients.values().map(Client::view).collect::<Vec<_>>(),
				})
			})
			.collect();
		json!({ "version": VERSION, "port": *PORT, "pid": std::process::id(), "projects": projects })
	}

	// -- commands: the CLI asks a project's Studio to do something (pull, snapshot) --

	fn settle(&self, id: &str, value: CommandResult) {
		if let Some((_, waiter)) = lock(&self.waiters).remove(id) {
			let _ = waiter.send(value);
		}
	}

	// A Studio went away: its commands will never be answered.
	fn abandon(&self, client: &str) {
		let mut waiters = lock(&self.waiters);
		let ids: Vec<String> = waiters.iter().filter(|(_, (c, _))| c == client).map(|(id, _)| id.clone()).collect();
		for id in ids {
			if let Some((_, waiter)) = waiters.remove(&id) {
				let _ = waiter.send(json!({ "ok": false, "error": "Studio disconnected before it answered" }));
			}
		}
	}

	// Queues a command for one Studio (`command_target`) and waits for its result.
	async fn run_command(
		&self,
		project: &Project,
		kind: &'static str,
		path: Option<Vec<String>>,
		file: Option<String>,
		timeout: Duration,
		from: Option<&str>,
	) -> Result<CommandResult, Reject> {
		let id = random_hex(16);
		let (tx, rx) = oneshot::channel();
		{
			let mut st = project.state();
			let target = command_target(st.clients.values().collect(), from)?;
			lock(&self.waiters).insert(id.clone(), (target.clone(), tx));
			st.commands.push(Command { id: id.clone(), client: target, kind, path, file });
		}
		project.wake();
		match tokio::time::timeout(timeout, rx).await {
			Ok(Ok(result)) => Ok(result),
			_ => {
				lock(&self.waiters).remove(&id);
				project.state().commands.retain(|c| c.id != id);
				Ok(json!({ "ok": false, "error": format!("Studio did not answer in {} s", timeout.as_secs()) }))
			}
		}
	}
}

// The Studio a command goes to: the one named by `from` (a copy's session or a place id), else the only
// one connected. Several Studios and no `from` is refused: the CLI can't know which one is meant.
fn command_target(live: Vec<&Client>, from: Option<&str>) -> Result<String, Reject> {
	if live.is_empty() {
		return Err(reject(409, "no Studio is connected to this project (open the place in Edit; studio-sync status)"));
	}
	let connected = live.iter().map(|c| c.describe()).collect::<Vec<_>>().join(", ");
	if let Some(from) = from {
		let matches: Vec<_> = live
			.iter()
			.filter(|c| match &c.session {
				Some(session) => session == from,
				None => c.place_id.to_string() == from,
			})
			.collect();
		return match matches.as_slice() {
			[one] => Ok(one.id.clone()),
			[] => Err(reject(409, format!("no connected Studio is {from} (connected: {connected})"))),
			many => Err(reject(409, format!("{} Studios are {from}: close all but one", many.len()))),
		};
	}
	match live.as_slice() {
		[one] => Ok(one.id.clone()),
		many => Err(reject(
			409,
			format!("{} Studios are connected to this project ({connected}): name one with --from <copy session or place id>", many.len()),
		)),
	}
}

// Runs disk work off the async threads.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, Reject> + Send + 'static) -> Result<T, Reject> {
	tokio::task::spawn_blocking(work).await.map_err(|e| reject(500, e.to_string()))?
}

// Browsers always send Origin on cross-site requests and WebSockets; Studio and the CLI never do.
// That, and the Host check (DNS rebinding), keep web pages from writing into a repo.
fn allowed(req: &Request<Incoming>) -> Result<(), Reject> {
	let host = req.headers().get("host").and_then(|h| h.to_str().ok()).unwrap_or_default();
	let host = match host.rsplit_once(':') {
		Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
		_ => host,
	};
	if req.headers().contains_key("origin") || !["localhost", "127.0.0.1", "[::1]"].contains(&host) {
		return Err(reject(403, "forbidden"));
	}
	Ok(())
}

// GET /socket?placeId&session&client&plugin&epoch&version: a Studio's connection, after its GET /tree.
fn open_socket(daemon: Arc<Daemon>, req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Reject> {
	let header = |name: &str| req.headers().get(name).and_then(|h| h.to_str().ok()).unwrap_or_default().to_ascii_lowercase();
	if !header("upgrade").contains("websocket") || !header("connection").contains("upgrade") {
		return Err(reject(426, "a WebSocket"));
	}
	let Some(key) = req.headers().get("sec-websocket-key").map(|k| derive_accept_key(k.as_bytes())) else {
		return Err(reject(400, "no Sec-WebSocket-Key"));
	};
	let q = Query::parse(req.uri().query());
	let project = daemon.by_place(q.get("placeId"), q.some("session"))?;
	let Some(client) = q.some("client").map(str::to_string) else {
		return Err(reject(400, "no client"));
	};
	let conn = project.connect(
		&client,
		q.number("placeId").unwrap_or(0),
		q.some("session"),
		q.get("plugin").unwrap_or("?"),
		q.number("version").unwrap_or(0),
	);
	let rung = Rung::new(q.get("epoch").unwrap_or_default(), q.number("version").unwrap_or(0));
	tokio::spawn(async move {
		if let Ok(upgraded) = hyper::upgrade::on(req).await {
			let socket = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
			talk(&project, &client, conn, rung, socket).await;
		}
		project.disconnect(&client, conn);
		daemon.abandon(&client);
	});
	let mut response = Response::new(Full::new(Bytes::new()));
	*response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
	let headers = response.headers_mut();
	headers.insert("upgrade", hyper::header::HeaderValue::from_static("websocket"));
	headers.insert("connection", hyper::header::HeaderValue::from_static("Upgrade"));
	headers.insert("sec-websocket-accept", key.parse().map_err(|_| reject(500, "accept key"))?);
	Ok(response)
}

// One open socket: sends what the project has for this client whenever it changes, and
// takes in what Studio reports, until either side closes it.
async fn talk(project: &Project, client: &str, conn: u64, mut rung: Rung, socket: WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>) {
	let (mut tx, mut rx) = socket.split();
	loop {
		let notified = project.changed().notified();
		tokio::pin!(notified);
		notified.as_mut().enable();
		let Some(outgoing) = project.outgoing(client, conn, &mut rung) else { return };
		for message in outgoing {
			if tx.send(Message::text(message)).await.is_err() {
				return;
			}
		}
		tokio::select! {
			_ = &mut notified => {}
			message = rx.next() => match message {
				Some(Ok(Message::Text(text))) => project.receive(client, &text),
				Some(Ok(Message::Close(_)) | Err(_)) | None => return,
				Some(Ok(_)) => {} // pings are answered by the socket itself
			},
		}
	}
}

async fn handle(daemon: Arc<Daemon>, req: Request<Incoming>) -> Handled {
	let q = Query::parse(req.uri().query());
	let route = req.uri().path().to_string();
	match *req.method() {
		Method::GET => get(daemon, &route, q).await,
		Method::POST => {
			let body = match Limited::new(req.into_body(), MAX_BODY).collect().await {
				Ok(body) => body.to_bytes(),
				Err(_) => return Err(reject(413, "body too large")),
			};
			post(daemon, &route, q, body).await
		}
		_ => Err(reject(405, "GET or POST")),
	}
}

async fn get(daemon: Arc<Daemon>, route: &str, q: Query) -> Handled {
	match route {
		"/status" => return Ok(Reply::json(200, &daemon.status())),
		"/plugin/sync.luau" => return Ok(Reply::text(200, crate::install::SYNC_SOURCE)),
		"/wait" => {
			let project = daemon.by_dir(q.get("dir"))?;
			let timeout = Duration::from_secs(q.number("timeout").unwrap_or(30));
			return Ok(Reply::json(200, &project.wait_applied(timeout).await));
		}
		_ => {}
	}
	if route.starts_with("/tools/") && route.ends_with(".luau") && !route.contains("..") {
		// files under a project's tools/, for scripts run in Studio
		let candidates = match daemon.by_place(q.get("placeId"), q.some("session")) {
			Ok(project) => vec![project],
			Err(_) => daemon.projects(),
		};
		let rel = route[1..].to_string();
		return blocking(move || {
			for candidate in candidates {
				let full = candidate.dir.join(&rel);
				if full.exists() {
					return Ok(Reply::text(200, read_text(&full)?));
				}
			}
			Err(reject(404, "no such file"))
		})
		.await;
	}

	let project = daemon.by_place(q.get("placeId"), q.some("session"))?;
	project.ready().await.map_err(|e| reject(503, e))?;
	match route {
		"/project" => Ok(Reply::json(200, &project.info())),
		// the whole tree is megabytes of JSON for a big project: off the async threads
		"/tree" => blocking(move || Ok(Reply::json(200, &project.tree()))).await,
		"/changes" => {
			// no `since` is newer than any version: a full sync
			let since = q.number("since").unwrap_or(u64::MAX);
			Ok(Reply::json(200, &project.changes(q.get("epoch").unwrap_or_default(), since)))
		}
		"/snapshot/plan" => {
			let config = project.state().config.clone();
			Ok(Reply::json(200, &snapshot::plan(&project.dir, &config)?))
		}
		"/model" => {
			let file = q.get("file").unwrap_or_default().to_string();
			let owned = (file.ends_with(".rbxm") || file.ends_with(".rbxmx"))
				&& project.state().places.values().any(|p| p.file.as_deref() == Some(Path::new(&file)));
			if !owned {
				return Err(reject(404, "not a model file of this project"));
			}
			blocking(move || Ok(Reply::text(200, BASE64.encode(read(&project.dir.join(file))?)))).await
		}
		_ => Err(reject(404, "unknown")),
	}
}

async fn post(daemon: Arc<Daemon>, route: &str, q: Query, body: Bytes) -> Handled {
	let text = || String::from_utf8_lossy(&body).into_owned();
	match route {
		// from the CLI
		"/pull/request" => {
			let project = daemon.by_dir(q.get("dir"))?;
			let file = q.some("file").map(str::to_string);
			let result = daemon.run_command(&project, "pull", Some(q.path("path")?), file, PULL_TIMEOUT, q.some("from")).await?;
			return Ok(command_reply(result));
		}
		"/snapshot/request" => {
			let project = daemon.by_dir(q.get("dir"))?;
			let result = daemon.run_command(&project, "snapshot", None, None, SNAPSHOT_TIMEOUT, q.some("from")).await?;
			return Ok(command_reply(result));
		}
		"/command/result" => {
			let result = match q.some("error") {
				Some(error) => json!({ "ok": false, "error": error }),
				None if body.is_empty() => json!({}),
				None => serde_json::from_slice(&body)?,
			};
			daemon.settle(q.get("id").unwrap_or_default(), result);
			return Ok(Reply::ok());
		}
		_ => {}
	}

	let project = daemon.by_place(q.get("placeId"), q.some("session"))?;
	match route {
		"/backup" => {
			let items: Vec<BackupItem> = serde_json::from_value(serde_json::from_slice::<Value>(&body)?["items"].take())?;
			blocking(move || Ok(Reply::json(200, &json!({ "dir": save_backup(&project.name, &items)? })))).await
		}
		"/pull" => {
			// from Studio: the serialized instance, base64, in parts of under 1 MB
			let id = q.get("id").unwrap_or_default().to_string();
			if let Some(error) = q.some("error") {
				daemon.settle(&id, json!({ "ok": false, "error": error }));
				return Ok(Reply::ok());
			}
			let token = format!("{}|{}", project.dir.display(), q.get("upload").unwrap_or("null"));
			let data = {
				let mut parts = lock(&daemon.pull_parts);
				parts.entry(token.clone()).or_default().push_str(&text());
				if q.get("last") != Some("1") {
					return Ok(Reply::text(200, "part"));
				}
				parts.remove(&token).unwrap_or_default()
			};
			project.ready().await.map_err(|e| reject(503, e))?;
			let path = q.path("path")?;
			let file = q.some("file").map(str::to_string);
			let saved = blocking(move || {
				let data = BASE64.decode(data.trim()).map_err(|e| reject(400, e.to_string()))?;
				let rel = {
					let mut st = project.state();
					Writer::new(&project.dir, &project.name, &mut st).save_pulled(&path, file.as_deref(), &data)
				};
				Ok(rel?)
			})
			.await;
			let result = match saved {
				Ok(file) => json!({ "ok": true, "file": file }),
				Err(e) => json!({ "ok": false, "error": e.message }),
			};
			daemon.settle(&id, result.clone());
			Ok(command_reply(result))
		}
		"/save" => {
			// a model edited in Studio: the serialized instance, base64, in parts of under 1 MB
			let token = format!("{}|save|{}", project.dir.display(), q.get("upload").unwrap_or("null"));
			let data = {
				let mut parts = lock(&daemon.pull_parts);
				parts.entry(token.clone()).or_default().push_str(&text());
				if q.get("last") != Some("1") {
					return Ok(Reply::text(200, "part"));
				}
				parts.remove(&token).unwrap_or_default()
			};
			project.ready().await.map_err(|e| reject(503, e))?;
			let path = q.path("path")?;
			let base = q.get("base").unwrap_or_default().to_string();
			let mode = q.get("mode").unwrap_or("edit").to_string();
			let place = match q.some("session") {
				Some(session) => format!("copy-{session}"),
				None => q.get("placeId").unwrap_or_default().to_string(),
			};
			blocking(move || {
				let data = BASE64.decode(data.trim()).map_err(|e| reject(400, e.to_string()))?;
				let result = {
					let mut st = project.state();
					Writer::new(&project.dir, &project.name, &mut st).save_model(&path, &place, &mode, &base, &data)
				};
				Ok(Reply::json(200, &result.unwrap_or_else(|e| json!({ "ok": false, "error": e.0 }))))
			})
			.await
		}
		"/snapshot/file" => {
			let token = q.get("upload").unwrap_or_default().to_string();
			let path = q.path("path")?;
			let last = q.get("last") == Some("1");
			let part = text();
			blocking(move || {
				let mut uploads = lock(&daemon.uploads);
				snapshot::receive_file(&mut uploads, &project.dir, &token, &path, &part, last)?;
				Ok(Reply::ok())
			})
			.await
		}
		"/snapshot/commit" => {
			let id = q.get("id").unwrap_or_default().to_string();
			let token = q.get("upload").unwrap_or_default().to_string();
			let config = project.state().config.clone();
			let waiting = daemon.clone();
			let committed = blocking(move || {
				let report = serde_json::from_slice(&body)?;
				let mut uploads = lock(&waiting.uploads);
				Ok(snapshot::commit(&mut uploads, &project.dir, &project.name, &config, &token, report)?)
			})
			.await;
			let result = committed.unwrap_or_else(|e| json!({ "ok": false, "error": e.message }));
			daemon.settle(&id, result.clone());
			Ok(command_reply(result))
		}
		_ if route.starts_with("/content/") => {
			let route = route.to_string();
			blocking(move || content(&project, &route, &q, &body)).await
		}
		_ => Err(reject(404, "unknown")),
	}
}

fn command_reply(result: Value) -> Reply {
	Reply::json(if result["ok"] == true { 200 } else { 422 }, &result)
}

// The bake receiver (`content` in studio-sync.json).
fn content(project: &Project, route: &str, q: &Query, body: &[u8]) -> Handled {
	let Some(cfg) = project.state().config.content.clone() else {
		return Err(reject(400, "this project has no \"content\" in studio-sync.json"));
	};
	let staging = &project.staging;
	if route == "/content/begin" {
		remove(staging)?;
		mkdirs(staging)?;
		return Ok(Reply::text(200, "ready"));
	}
	if let Some((service, name)) = content_file(route) {
		if !staging.exists() {
			return Err(reject(409, "POST /content/begin first"));
		}
		let folder = staging.join(service);
		mkdirs(&folder)?;
		let data = BASE64.decode(String::from_utf8_lossy(body).trim()).map_err(|e| reject(400, e.to_string()))?;
		// part 1 starts the file, later parts append in order
		let file = folder.join(format!("{name}.rbxm"));
		if matches!(q.get("part"), None | Some("1")) {
			write(&file, &data)?;
		} else {
			use std::io::Write;
			let mut out = std::fs::OpenOptions::new().append(true).open(&file).map_err(|e| Error(format!("{}: {e}", file.display())))?;
			out.write_all(&data).map_err(|e| Error(format!("{}: {e}", file.display())))?;
		}
		return Ok(Reply::text(200, data.len().to_string()));
	}
	if route == "/content/commit" {
		if !staging.exists() {
			return Err(reject(409, "POST /content/begin first"));
		}
		let mut sources = cfg.sources.clone();
		sources.sort();
		let mut hashes = serde_json::Map::new();
		for source in sources {
			let digest = Sha256::digest(read(&project.dir.join(&source))?);
			hashes.insert(source, Value::String(hex(&digest)));
		}
		let manifest = format!("{}\n", serde_json::to_string_pretty(&json!({ "sources": hashes }))?);
		write(&staging.join("bake.json"), manifest)?;
		let target = project.dir.join(&cfg.dir);
		remove(&target)?;
		mkdirs(target.parent().unwrap_or(&project.dir))?;
		crate::util::copy(staging, &target)?;
		remove(staging)?;
		let mut files = Vec::new();
		files_under(&target, &mut files)?;
		let mut listed: Vec<String> = files.iter().map(|f| slashes(f.strip_prefix(&project.dir).unwrap_or(f))).collect();
		listed.sort();
		return Ok(Reply::text(200, listed.join("\n")));
	}
	Err(reject(404, "unknown"))
}

// /content/file/<Service>/<Name>.rbxm
fn content_file(route: &str) -> Option<(&str, &str)> {
	let (service, file) = route.strip_prefix("/content/file/")?.split_once('/')?;
	let name = file.strip_suffix(".rbxm")?;
	let ok_service = !service.is_empty() && service.bytes().all(|b| b.is_ascii_alphabetic());
	let ok_name = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
	(ok_service && ok_name).then_some((service, name))
}

async fn respond(daemon: Arc<Daemon>, req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
	let what = format!("{} {}", req.method(), req.uri());
	let response = match allowed(&req) {
		Err(e) => Err(e),
		Ok(()) if req.uri().path() == "/socket" => open_socket(daemon, req),
		Ok(()) => handle(daemon, req).await.map(Reply::response),
	};
	Ok(response.unwrap_or_else(|e| {
		if e.status == 500 {
			log!("{what}: {}", e.message);
		}
		Reply::text(e.status, e.message).response()
	}))
}

// projects.json changes reach the daemon as file events.
fn watch_registry(daemon: Arc<Daemon>) -> notify::Result<notify::RecommendedWatcher> {
	let file = projects_file();
	let dir = crate::paths::real(&STATE_DIR).unwrap_or_else(|_| STATE_DIR.clone());
	let watched = dir.join("projects.json");
	let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
		if let Ok(event) = event
			&& event.paths.iter().any(|p| *p == watched || *p == file)
		{
			daemon.refresh_registry();
		}
	})?;
	watcher.watch(&dir, RecursiveMode::NonRecursive)?;
	Ok(watcher)
}

pub fn serve() -> Result<(), Error> {
	mkdirs(&STATE_DIR)?;
	if !std::io::stdout().is_terminal() {
		crate::log::to_file();
	}
	let daemon = Arc::new(Daemon::default());
	daemon.refresh_registry();
	let _registry = watch_registry(daemon.clone()).map_err(|e| Error(format!("watch {}: {e}", STATE_DIR.display())))?;
	let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().map_err(|e| Error(e.to_string()))?;
	runtime.block_on(listen(daemon))
}

async fn listen(daemon: Arc<Daemon>) -> Result<(), Error> {
	let listener = match TcpListener::bind(("127.0.0.1", *PORT)).await {
		Ok(listener) => listener,
		Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
			log!("port {} is in use: another studio-sync is running. `studio-sync restart`.", *PORT);
			std::process::exit(1);
		}
		Err(e) => return Err(Error(e.to_string())),
	};
	log!("studio-sync {VERSION} serving on http://localhost:{} (pid {})", *PORT, std::process::id());
	loop {
		let Ok((stream, _)) = listener.accept().await else { continue };
		let daemon = daemon.clone();
		tokio::spawn(async move {
			let service = service_fn(move |req| respond(daemon.clone(), req));
			let _ = http1::Builder::new().timer(TokioTimer::new()).serve_connection(TokioIo::new(stream), service).with_upgrades().await;
		});
	}
}
