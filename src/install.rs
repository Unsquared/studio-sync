// Installing studio-sync on this machine: the Studio plugin (an .rbxmx written
// straight from plugin/, no other tool needed), the background service (a macOS
// LaunchAgent, or a Windows Startup-folder launcher), and the Claude Code skill.

use crate::paths::{LABEL, LEGACY_LABELS, home, log_file, plugin_dir};
use crate::util::{Error, Res, mkdirs, remove, write};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread::sleep;
use std::time::Duration;

// Built into the binary, so the plugin and the skill always match the daemon.
pub const SYNC_SOURCE: &str = include_str!("../plugin/Sync.luau");
const MAIN_SOURCE: &str = include_str!("../plugin/init.server.luau");
const SKILL: &str = include_str!("../skill/SKILL.md");
const CONFIG_DOC: &str = include_str!("../docs/config.md");

fn plugin_file() -> PathBuf {
	plugin_dir().join("StudioSync.rbxmx")
}

fn cdata(text: &str) -> String {
	format!("<![CDATA[{}]]>", text.replace("]]>", "]]]]><![CDATA[>"))
}

fn script_item(class_name: &str, name: &str, source: &str, children: &str) -> String {
	format!(
		"<Item class=\"{class_name}\" referent=\"RBX{name}\"><Properties><string name=\"Name\">{name}</string><ProtectedString name=\"Source\">{}</ProtectedString></Properties>{children}</Item>",
		cdata(source)
	)
}

// The plugin as one .rbxmx: the plugin Script with Sync as its child ModuleScript.
pub fn build_plugin() -> Res<PathBuf> {
	let sync = script_item("ModuleScript", "Sync", SYNC_SOURCE, "");
	let main = script_item("Script", "StudioSync", MAIN_SOURCE, &sync);
	mkdirs(&plugin_dir())?;
	write(&plugin_file(), format!("<roblox version=\"4\">{main}</roblox>\n"))?;
	Ok(plugin_file())
}

// This binary, where the service starts it from.
fn binary() -> Res<PathBuf> {
	env::current_exe().map_err(|e| Error(format!("where is studio-sync? {e}")))
}

fn launchctl(args: &[&str]) -> Output {
	Command::new("launchctl").args(args).stdin(Stdio::null()).output().expect("launchctl runs on macOS")
}

fn xml_escape(text: &str) -> String {
	text.replace('&', "&amp;").replace('<', "&lt;")
}

fn domain() -> String {
	let uid = Command::new("id").arg("-u").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
	format!("gui/{}", uid.unwrap_or_default())
}

fn plist_path(label: &str) -> PathBuf {
	home().join("Library").join("LaunchAgents").join(format!("{label}.plist"))
}

fn windows_startup() -> PathBuf {
	PathBuf::from(env::var_os("APPDATA").unwrap_or_default())
		.join("Microsoft")
		.join("Windows")
		.join("Start Menu")
		.join("Programs")
		.join("Startup")
}

fn install_service() -> Res<String> {
	let exe = binary()?;
	if cfg!(target_os = "macos") {
		for label in LEGACY_LABELS {
			launchctl(&["bootout", &format!("{}/{label}", domain())]);
			remove(&plist_path(label))?;
		}
		let home = home();
		let home = home.display();
		// a project's `copy` command runs from the daemon: the usual tool homes, since a LaunchAgent gets a bare PATH
		let path = [
			format!("{home}/.rokit/bin"),
			format!("{home}/.aftman/bin"),
			format!("{home}/.cargo/bin"),
			format!("{home}/.local/bin"),
			"/opt/homebrew/bin".into(),
			"/usr/local/bin".into(),
			"/usr/bin".into(),
			"/bin".into(),
		]
		.join(":");
		let log = xml_escape(&log_file().to_string_lossy());
		let plist = format!(
			r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{}</string>
		<string>serve</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>EnvironmentVariables</key>
	<dict>
		<key>PATH</key>
		<string>{}</string>
		<key>HOME</key>
		<string>{}</string>
	</dict>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
	<key>ProcessType</key>
	<string>Background</string>
</dict>
</plist>
"#,
			xml_escape(&exe.to_string_lossy()),
			xml_escape(&path),
			xml_escape(&home.to_string()),
		);
		let file = plist_path(LABEL);
		mkdirs(file.parent().unwrap_or(&file))?;
		write(&file, plist)?;
		let target = format!("{}/{LABEL}", domain());
		launchctl(&["bootout", &target]);
		// bootout returns before the old job is gone; bootstrap fails (EIO) until it is
		let file = file.to_string_lossy();
		let mut loaded = launchctl(&["bootstrap", &domain(), &file]);
		for _ in 0..20 {
			if loaded.status.success() {
				break;
			}
			sleep(Duration::from_millis(250));
			loaded = launchctl(&["bootstrap", &domain(), &file]);
		}
		if !loaded.status.success() {
			crate::bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&loaded.stderr).trim());
		}
		return Ok(format!("LaunchAgent {LABEL} (starts at login, restarted if it dies; log {})", log_file().display()));
	}
	if cfg!(windows) {
		// a hidden window runs a loop that restarts the daemon if it exits
		let dir = home().join(".config").join("studio-sync");
		mkdirs(&dir)?;
		let cmd = dir.join("run-daemon.cmd");
		// not the log file: the redirect locks its file, and the daemon's own log then fails.
		// ping, not `timeout`, which didn't wait in the launcher's hidden window.
		let crash_log = dir.join("daemon.out.log");
		write(
			&cmd,
			format!(
				"@echo off\r\n:loop\r\n\"{}\" serve >> \"{}\" 2>&1\r\nping -n 4 127.0.0.1 > nul\r\ngoto loop\r\n",
				exe.display(),
				crash_log.display()
			),
		)?;
		let vbs = windows_startup().join("studio-sync.vbs");
		write(&vbs, format!("CreateObject(\"WScript.Shell\").Run \"\"\"{}\"\"\", 0, False\r\n", cmd.display()))?;
		Command::new("wscript.exe").arg(&vbs).spawn().map_err(|e| Error(format!("wscript: {e}")))?;
		return Ok(format!("Startup launcher {} (starts at login, restarted if it dies; log {})", vbs.display(), log_file().display()));
	}
	Ok("no background service on this OS: run `studio-sync serve` in a terminal".into())
}

fn uninstall_service() -> Res<()> {
	if cfg!(target_os = "macos") {
		for label in std::iter::once(&LABEL).chain(LEGACY_LABELS) {
			launchctl(&["bootout", &format!("{}/{label}", domain())]);
			remove(&plist_path(label))?;
		}
	} else if cfg!(windows) {
		remove(&windows_startup().join("studio-sync.vbs"))?;
		let _ = Command::new("powershell")
			.args([
				"-NoProfile",
				"-Command",
				"Get-CimInstance Win32_Process -Filter \"CommandLine like '%run-daemon.cmd%'\" | Invoke-CimMethod -MethodName Terminate",
			])
			.output();
	}
	Ok(())
}

fn skill_dir() -> PathBuf {
	home().join(".claude").join("skills").join("studio-sync")
}

fn is_link(path: &Path) -> bool {
	fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink() || is_junction(&m))
}

#[cfg(windows)]
fn is_junction(meta: &fs::Metadata) -> bool {
	use std::os::windows::fs::MetadataExt;
	const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
	meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_junction(_: &fs::Metadata) -> bool {
	false
}

// The skill as a file of its own, so it matches this binary.
fn install_skill() -> Res<String> {
	let dir = skill_dir();
	// earlier versions linked the skill to the checkout
	if is_link(&dir) {
		fs::remove_file(&dir).or_else(|_| fs::remove_dir(&dir)).map_err(|e| Error(format!("{}: {e}", dir.display())))?;
	}
	mkdirs(&dir)?;
	let file = dir.join("SKILL.md");
	write(&file, SKILL)?;
	write(&dir.join("config.md"), CONFIG_DOC)?;
	Ok(file.display().to_string())
}

fn uninstall_skill() -> Res<()> {
	let dir = skill_dir();
	if is_link(&dir) {
		return fs::remove_file(&dir).or_else(|_| fs::remove_dir(&dir)).map_err(|e| Error(format!("{}: {e}", dir.display())));
	}
	remove(&dir.join("SKILL.md"))?;
	remove(&dir.join("config.md"))?;
	let _ = fs::remove_dir(&dir); // only when nothing else is in it
	Ok(())
}

// Where a link to the command goes on macOS: a shell that reads no ~/.zshrc, as an agent's often
// doesn't, finds it here, and not in ~/.cargo/bin.
fn link_dirs() -> [PathBuf; 3] {
	[PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin"), home().join(".local").join("bin")]
}

#[cfg(unix)]
fn link_command() -> Res<Option<String>> {
	let exe = binary()?;
	if !cfg!(target_os = "macos") || link_dirs().iter().any(|dir| exe.parent() == Some(dir)) {
		return Ok(None);
	}
	for dir in link_dirs().iter().filter(|dir| dir.is_dir()) {
		let link = dir.join("studio-sync");
		if link.exists() && !is_link(&link) {
			continue; // not ours
		}
		if is_link(&link) && fs::remove_file(&link).is_err() {
			continue;
		}
		if std::os::unix::fs::symlink(&exe, &link).is_ok() {
			return Ok(Some(format!("command: {} -> {}", link.display(), exe.display())));
		}
	}
	Ok(None)
}

#[cfg(not(unix))]
fn link_command() -> Res<Option<String>> {
	Ok(None)
}

fn unlink_command() -> Res<()> {
	let exe = binary()?;
	for link in link_dirs().iter().map(|dir| dir.join("studio-sync")) {
		if fs::read_link(&link).is_ok_and(|target| target == exe) {
			remove(&link)?;
		}
	}
	Ok(())
}

// An executable of that name in a PATH directory. Not run: a rokit shim fails outside a project.
fn on_path(program: &str) -> bool {
	let names = if cfg!(windows) { vec![format!("{program}.exe"), format!("{program}.cmd")] } else { vec![program.to_string()] };
	env::var_os("PATH").is_some_and(|path| env::split_paths(&path).any(|dir| names.iter().any(|n| dir.join(n).is_file())))
}

pub fn install() -> Res<Vec<String>> {
	let mut lines = Vec::new();
	lines.push(format!("plugin: {} (Studio picks it up on its next start, often live)", build_plugin()?.display()));
	lines.push(format!("service: {}", install_service()?));
	lines.push(format!("Claude skill: {}", install_skill()?));
	let linked = link_command()?;
	lines.extend(linked.clone());
	if linked.is_none() && !on_path("studio-sync") {
		lines.push("the `studio-sync` command is not on PATH yet: `rokit add --global Unsquared/studio-sync`".into());
	}
	Ok(lines)
}

// Installs the new binary the way this one got here, then has the new one run `install`: the old
// one carries the old plugin and skill.
pub fn update() -> Res<Vec<String>> {
	let exe = binary()?;
	let from_rokit = exe.components().any(|c| c.as_os_str() == ".rokit");
	let in_checkout = fs::read_to_string("Cargo.toml").is_ok_and(|manifest| manifest.contains("name = \"studio-sync\""));
	let (program, args): (&str, &[&str]) = if from_rokit {
		("rokit", &["update", "--global", "studio-sync"])
	} else if in_checkout {
		("cargo", &["install", "--path", ".", "--locked"])
	} else {
		("cargo", &["install", "--git", "https://github.com/Unsquared/studio-sync", "--locked"])
	};
	let mut lines = vec![format!("{program} {}", args.join(" "))];
	let run = |command: &mut Command, what: &str| -> Res<()> {
		let status = command.status().map_err(|e| Error(format!("{what}: {e}")))?;
		if !status.success() {
			crate::bail!("{what} failed ({status}); nothing else changed");
		}
		Ok(())
	};
	run(Command::new(program).args(args), program)?;
	run(Command::new(&exe).arg("install"), "the new binary's install")?;
	lines.push("updated. A Studio that `status` flags needs a restart for the new plugin.".into());
	Ok(lines)
}

pub fn uninstall() -> Res<Vec<String>> {
	uninstall_service()?;
	remove(&plugin_file())?;
	uninstall_skill()?;
	unlink_command()?;
	Ok(vec!["removed the service, the plugin and the skill link (`cargo uninstall studio-sync` removes the command)".into()])
}
