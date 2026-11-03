// Opening Studio for an agent: the project's dev place (`open`), or a private
// local copy built from this checkout (`open --copy`) that syncs back to it.
// A copy is a .rbxl with a session id in a Workspace attribute; the plugin
// sends it, and the daemon maps it to the checkout that made the copy.

use crate::client;
use crate::config::Config;
use crate::paths::{backup_root, copies_dir, home, local_app_data};
use crate::registry::{Registry, Session};
use crate::snapshot::{BuildOptions, build, run};
use crate::util::{Error, Res, copy, iso_now, mkdirs, random_hex, remove, stamp};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

// In the background: Studio doesn't take focus from whatever the user is doing.
fn launch(target: &str, is_file: bool) -> Res<()> {
	let mut command = if cfg!(target_os = "macos") {
		// -n: a new Studio even when one is running; a file given to `open -a` alone lands on the start page
		let app = [PathBuf::from("/Applications"), home().join("Applications")]
			.into_iter()
			.map(|d| d.join("RobloxStudio.app"))
			.find(|a| a.exists());
		let mut command = Command::new("open");
		if is_file {
			// by name, `open -a RobloxStudio` doesn't start a second Studio
			let app = app.ok_or("RobloxStudio.app is not in /Applications or ~/Applications")?;
			command.args(["-g", "-n", "-a"]).arg(app).args(["--args", "-task", "EditFile", "-localPlaceFile", target]);
		} else {
			command.args(["-g", target]);
		}
		command
	} else if cfg!(windows) {
		windows_launch(target, is_file)?
	} else {
		crate::bail!("opening Studio is supported on macOS and Windows");
	};
	command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
	command.spawn().map_err(|e| Error(format!("launch Studio: {e}")))?;
	Ok(())
}

// `start /min` opens minimized and without focus. A file goes straight to Studio's exe:
// through the file association it can land on the start page, as on macOS.
#[cfg(windows)]
fn windows_launch(target: &str, is_file: bool) -> Res<Command> {
	use std::os::windows::process::CommandExt;
	let line = if is_file {
		let exe = windows_studio()
			.ok_or("RobloxStudioBeta.exe not found under %LOCALAPPDATA%\\Roblox\\Versions (open Studio once to install it)")?;
		format!("\"{}\" -task EditFile -localPlaceFile \"{target}\"", exe.display())
	} else {
		format!("\"{target}\"")
	};
	// Off the caller's console and process group: attached, Studio dies with the terminal
	// (or agent shell) that ran `open`, a second after it connects.
	const DETACHED_PROCESS: u32 = 0x0000_0008;
	const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
	// The child inherits every inheritable handle, and this process's own stdio came in
	// inheritable: Studio would hold the caller's output pipe open, so a shell reading it
	// (`studio-sync open --copy | tail`, an agent's tool call) waits until Studio exits.
	#[link(name = "kernel32")]
	unsafe extern "system" {
		fn GetStdHandle(which: u32) -> isize;
		fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
	}
	const HANDLE_FLAG_INHERIT: u32 = 1;
	for which in [-10i32, -11, -12] {
		// SAFETY: plain Win32 calls on this process's standard handles; a failure leaves the flag as it was.
		unsafe {
			let handle = GetStdHandle(which as u32);
			if handle != 0 && handle != -1 {
				SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
			}
		}
	}
	let mut command = Command::new("cmd");
	command.raw_arg(format!("/c start \"\" /min {line}")).creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
	Ok(command)
}

#[cfg(not(windows))]
fn windows_launch(_: &str, _: bool) -> Res<Command> {
	unreachable!()
}

// The newest installed Studio: %LOCALAPPDATA%\Roblox\Versions\version-*\RobloxStudioBeta.exe.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_studio() -> Option<PathBuf> {
	let versions = local_app_data().join("Roblox").join("Versions");
	fs::read_dir(versions)
		.ok()?
		.flatten()
		.map(|v| v.path().join("RobloxStudioBeta.exe"))
		.filter_map(|exe| Some((fs::metadata(&exe).ok()?.modified().ok()?, exe)))
		.max()
		.map(|(_, exe)| exe)
}

// Waits until a Studio matching `matches` is connected to the project at `dir` and has applied its version.
fn wait_connected(dir: &Path, matches: impl Fn(&Value) -> bool, timeout: Duration) -> Res<String> {
	let deadline = Instant::now() + timeout;
	loop {
		let status = client::status()?;
		let project = status["projects"].as_array().into_iter().flatten().find(|p| p["dir"].as_str() == dir.to_str());
		let client = project.and_then(|p| p["clients"].as_array()?.iter().find(|c| matches(c)));
		if let (Some(project), Some(client)) = (project, client)
			&& client["version"] == project["version"]
		{
			let errors: Vec<&str> = client["errors"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
			return Ok(if errors.is_empty() {
				format!("connected and in sync (v{})", project["version"])
			} else {
				format!("connected with errors:\n  {}", errors.join("\n  "))
			});
		}
		if Instant::now() > deadline {
			crate::bail!(
				"{}",
				match (project, client) {
					(Some(project), Some(_)) => format!("Studio connected but has not applied v{} yet", project["version"]),
					_ => format!(
						"Studio did not connect within {}s (is the plugin installed, and HTTP allowed? studio-sync status)",
						timeout.as_secs()
					),
				}
			);
		}
		sleep(Duration::from_millis(200));
	}
}

fn universe_of(place_id: u64) -> Res<u64> {
	let (status, body) = client::fetch_json(&format!("https://apis.roblox.com/universes/v1/places/{place_id}/universe"))?;
	if !(200..300).contains(&status) {
		crate::bail!("could not look up the universe of place {place_id}: {status}");
	}
	body["universeId"].as_u64().ok_or_else(|| Error(format!("place {place_id} has no universe (is it published?)")))
}

pub fn open_place(dir: &Path, place_id: u64, timeout: Duration) -> Res<Vec<String>> {
	let is_place = |c: &Value| c["placeId"] == place_id && c["session"].is_null();
	let status = client::status()?;
	let open = status["projects"]
		.as_array()
		.into_iter()
		.flatten()
		.filter(|p| p["dir"].as_str() == dir.to_str())
		.any(|p| p["clients"].as_array().is_some_and(|cs| cs.iter().any(is_place)));
	if open {
		return Ok(vec![format!("place {place_id} is already open and connected")]);
	}
	let universe_id = universe_of(place_id)?;
	launch(&format!("roblox-studio:1+launchmode:edit+task:EditPlace+placeId:{place_id}+universeId:{universe_id}"), false)?;
	let state = wait_connected(dir, is_place, timeout)?;
	Ok(vec![format!("opened place {place_id}: {state}")])
}

// A project that builds its own full place (`copy`): run its command; the copy is its output,
// under a name that carries the session (the plugin reads it from game.Name).
fn build_with_command(dir: &Path, copy: &crate::config::Copy, file: &Path) -> Res<Vec<String>> {
	let Some((command, args)) = copy.command.split_first().filter(|_| !copy.output.is_empty()) else {
		crate::bail!(
			"studio-sync.json \"copy\" needs \"command\" (a list: the program, then its arguments) and \"output\" (the place file it writes)"
		);
	};
	let shown = copy.command.join(" ");
	run(command, &args.iter().map(String::as_str).collect::<Vec<_>>(), dir)?;
	let output = dir.join(&copy.output);
	if !output.exists() {
		crate::bail!("{shown} did not write {}", copy.output);
	}
	crate::util::copy(&output, file)?;
	Ok(vec![format!("built {} with {shown}", copy.output)])
}

pub fn open_copy(dir: &Path, config: &Config, code_only: bool, timeout: Duration) -> Res<Vec<String>> {
	let session = random_hex(4);
	mkdirs(&copies_dir())?;
	let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
	let file = copies_dir().join(format!("{name}-copy-{session}.rbxl"));
	let mut lines = match (&config.copy, code_only) {
		(Some(copy), false) => build_with_command(dir, copy, &file)?,
		_ => build(dir, config, &BuildOptions { output: file.clone(), snapshot: !code_only })?,
	};
	let mut registry = Registry::read()?;
	registry.add(dir);
	registry.sessions.insert(session.clone(), Session { dir: dir.to_path_buf(), file: file.clone(), created_at: iso_now() });
	registry.write()?;
	launch(&file.to_string_lossy(), true)?;
	let state = wait_connected(dir, |c| c["session"] == session.as_str(), timeout)?;
	let file_name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
	lines.push(format!("opened copy {session} ({file_name}): {state}"));
	lines.push("Studio edits in this copy sync to this checkout only".into());
	Ok(lines)
}

// Copies whose Studio is gone are deleted with `clean`.
pub fn copies(clean: bool) -> Res<Vec<String>> {
	let mut registry = Registry::read()?;
	let live: HashSet<String> = client::status()
		.map(|status| {
			status["projects"]
				.as_array()
				.into_iter()
				.flatten()
				.flat_map(|p| p["clients"].as_array().cloned().unwrap_or_default())
				.filter_map(|c| c["session"].as_str().map(str::to_string))
				.collect()
		})
		.unwrap_or_default();
	let mut lines = Vec::new();
	let mut closed = Vec::new();
	for (id, session) in &registry.sessions {
		let open = live.contains(id);
		if clean && !open {
			remove(&session.file)?;
			closed.push(id.clone());
			lines.push(format!("removed {id} ({})", session.file.file_name().unwrap_or_default().to_string_lossy()));
		} else {
			lines.push(format!("{id}  {}  {}  {}", if open { "open" } else { "closed" }, session.dir.display(), session.created_at));
		}
	}
	if clean {
		for id in closed {
			registry.sessions.remove(&id);
		}
		registry.write()?;
	}
	Ok(if lines.is_empty() { vec!["no copies".into()] } else { lines })
}

// Studio processes with `file` on their command line.
fn studios_with(system: &System, file: &Path) -> Vec<sysinfo::Pid> {
	let file = file.as_os_str();
	system
		.processes()
		.iter()
		.filter(|(_, p)| p.name().to_string_lossy().to_lowercase().starts_with("robloxstudio"))
		.filter(|(_, p)| p.cmd().iter().any(|arg| arg == file || arg.to_string_lossy().contains(&*file.to_string_lossy())))
		.map(|(pid, _)| *pid)
		.collect()
}

// Closes copies (no save: code is already on disk; snapshot first to keep anything else),
// deletes their files and sessions. `which`: a session id, else every copy of `dir`.
pub fn close_copies(dir: &Path, which: Option<&str>) -> Res<Vec<String>> {
	let mut registry = Registry::read()?;
	let ids: Vec<String> = registry
		.sessions
		.iter()
		.filter(|(id, s)| match which {
			Some(which) => id.as_str() == which,
			None => s.dir == dir,
		})
		.map(|(id, _)| id.clone())
		.collect();
	if ids.is_empty() {
		return Ok(vec![match which {
			Some(which) => format!("no copy {which}"),
			None => "no copies of this checkout".into(),
		}]);
	}
	let mut system = System::new().map_err(|e| Error(format!("list processes: {e}")))?;
	system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always));
	let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
	let mut lines = Vec::new();
	for id in ids {
		let Some(session) = registry.sessions.remove(&id) else { continue };
		let pids = studios_with(&system, &session.file);
		// No graceful close: Studio ignores SIGTERM on macOS, and a built copy always gets a
		// "Save changes?" prompt. A copy is disposable; its code is on disk already.
		for pid in &pids {
			if let Some(process) = system.process(*pid) {
				process.kill();
			}
		}
		sleep(Duration::from_millis(300));
		if let Some(moved) = move_recovery_files(&session.file, &name)? {
			lines.push(moved);
		}
		remove(&session.file)?;
		let mut lock = session.file.clone().into_os_string();
		lock.push(".lock");
		remove(Path::new(&lock))?;
		let file_name = session.file.file_name().unwrap_or_default().to_string_lossy().into_owned();
		lines.push(format!("closed {id}{}, deleted {file_name}", if pids.is_empty() { " (no Studio had it open)" } else { "" }));
	}
	registry.write()?;
	Ok(lines)
}

// Where Studio writes auto-recovery files; it has moved them between versions.
fn auto_save_dirs() -> [PathBuf; 2] {
	let documents = home().join("Documents").join("ROBLOX").join("AutoSaves");
	if cfg!(windows) {
		[local_app_data().join("Roblox").join("RobloxStudio").join("AutoSaves"), documents]
	} else {
		[home().join("Library").join("Application Support").join("Roblox").join("RobloxStudio").join("AutoSaves"), documents]
	}
}

// Studio deletes `<place>_AutoRecovery_<n>.rbxl` only when it closes by itself. After a kill,
// the next Studio would offer to recover the copy, so they go to backups (they are normal
// place files, in case a copy had unsaved work).
fn move_recovery_files(file: &Path, project: &str) -> Res<Option<String>> {
	let stem = file.file_stem().unwrap_or_default().to_string_lossy();
	let prefix = format!("{stem}_AutoRecovery_").to_lowercase();
	let found: Vec<PathBuf> = auto_save_dirs()
		.iter()
		.filter_map(|d| fs::read_dir(d).ok())
		.flatten()
		.flatten()
		.filter(|e| e.file_name().to_string_lossy().to_lowercase().starts_with(&prefix))
		.map(|e| e.path())
		.collect();
	if found.is_empty() {
		return Ok(None);
	}
	let target = backup_root().join(project).join(format!("{}-recovery", stamp()));
	mkdirs(&target)?;
	for from in &found {
		let to = target.join(from.file_name().unwrap_or_default());
		// a killed process can hold the file a moment longer on Windows
		let mut tries = 0;
		loop {
			match copy(from, &to).and_then(|_| remove(from)) {
				Ok(()) => break,
				Err(e) if tries >= 20 => return Err(e),
				Err(_) => {
					tries += 1;
					sleep(Duration::from_millis(250));
				}
			}
		}
	}
	Ok(Some(format!("moved {} auto-recovery file(s) to {}", found.len(), target.display())))
}
