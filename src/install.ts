// Installing studio-sync on this machine: the Studio plugin (an .rbxmx written
// straight from plugin/, no Rojo needed), the background service (a macOS
// LaunchAgent, or a Windows Startup-folder launcher), and the Claude Code skill.

import { execFileSync, spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { LABEL, LEGACY_LABELS, LOG_FILE, PLUGIN_SOURCE_DIR, ROOT, SKILL_DIR, pluginDir } from "./paths.ts";

const CLI = path.join(ROOT, "src", "cli.ts");
const PLUGIN_FILE = () => path.join(pluginDir(), "StudioSync.rbxmx");

function cdata(text: string): string {
	return `<![CDATA[${text.split("]]>").join("]]]]><![CDATA[>")}]]>`;
}

function scriptItem(className: string, name: string, source: string, children = ""): string {
	return `<Item class="${className}" referent="RBX${name}"><Properties><string name="Name">${name}</string><ProtectedString name="Source">${cdata(source)}</ProtectedString></Properties>${children}</Item>`;
}

// The plugin as one .rbxmx: the plugin Script with Sync as its child ModuleScript.
export function buildPlugin(): string {
	const read = (file: string) => fs.readFileSync(path.join(PLUGIN_SOURCE_DIR, file), "utf8");
	const sync = scriptItem("ModuleScript", "Sync", read("Sync.luau"));
	const main = scriptItem("Script", "StudioSync", read("init.server.luau"), sync);
	const xml = `<roblox version="4">${main}</roblox>\n`;
	fs.mkdirSync(pluginDir(), { recursive: true });
	fs.writeFileSync(PLUGIN_FILE(), xml);
	// the Python version's plugin would fight this one
	for (const old of ["StudioSync.rbxm"]) fs.rmSync(path.join(pluginDir(), old), { force: true });
	return PLUGIN_FILE();
}

// The node on PATH (not its resolved Cellar path, which a brew upgrade deletes).
function nodeOnPath(): string {
	const finder = process.platform === "win32" ? "where" : "which";
	const found = spawnSync(finder, ["node"], { encoding: "utf8" }).stdout?.split(/\r?\n/)[0]?.trim();
	return found || process.execPath;
}

function sleep(ms: number): void {
	Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function launchctl(...args: string[]) {
	return spawnSync("launchctl", args, { encoding: "utf8" });
}

function plist(values: Record<string, unknown>): string {
	const encode = (v: unknown, indent: string): string => {
		if (typeof v === "boolean") return `${indent}<${v}/>`;
		if (typeof v === "string") return `${indent}<string>${v.replace(/&/g, "&amp;").replace(/</g, "&lt;")}</string>`;
		if (Array.isArray(v)) return `${indent}<array>\n${v.map((x) => encode(x, indent + "\t")).join("\n")}\n${indent}</array>`;
		const entries = Object.entries(v as Record<string, unknown>);
		return `${indent}<dict>\n${entries.map(([k, x]) => `${indent}\t<key>${k}</key>\n${encode(x, indent + "\t")}`).join("\n")}\n${indent}</dict>`;
	};
	return `<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n<plist version="1.0">\n${encode(values, "")}\n</plist>\n`;
}

const domain = () => `gui/${os.userInfo().uid}`;
const plistPath = (label: string) => path.join(os.homedir(), "Library", "LaunchAgents", `${label}.plist`);

function windowsStartup(): string {
	return path.join(process.env.APPDATA ?? "", "Microsoft", "Windows", "Start Menu", "Programs", "Startup");
}

function installService(): string {
	const node = nodeOnPath();
	if (process.platform === "darwin") {
		for (const label of LEGACY_LABELS) {
			launchctl("bootout", `${domain()}/${label}`);
			fs.rmSync(plistPath(label), { force: true });
		}
		const home = os.homedir();
		const pathEnv = [path.dirname(node), `${home}/.rokit/bin`, `${home}/.aftman/bin`, `${home}/.local/bin`, "/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];
		fs.mkdirSync(path.dirname(plistPath(LABEL)), { recursive: true });
		fs.writeFileSync(
			plistPath(LABEL),
			plist({
				Label: LABEL,
				ProgramArguments: [node, CLI, "serve"],
				RunAtLoad: true,
				KeepAlive: true,
				EnvironmentVariables: { PATH: [...new Set(pathEnv)].join(":"), HOME: home },
				StandardOutPath: LOG_FILE,
				StandardErrorPath: LOG_FILE,
				ProcessType: "Background",
			}),
		);
		launchctl("bootout", `${domain()}/${LABEL}`);
		// bootout returns before the old job is gone; bootstrap fails (EIO) until it is
		let loaded = launchctl("bootstrap", domain(), plistPath(LABEL));
		for (let tries = 0; loaded.status !== 0 && tries < 20; tries++) {
			sleep(250);
			loaded = launchctl("bootstrap", domain(), plistPath(LABEL));
		}
		if (loaded.status !== 0) throw new Error(`launchctl bootstrap failed: ${loaded.stderr.trim()}`);
		return `LaunchAgent ${LABEL} (starts at login, restarted if it dies; log ${LOG_FILE})`;
	}
	if (process.platform === "win32") {
		// a hidden window runs a loop that restarts the daemon if it exits
		const dir = path.join(os.homedir(), ".config", "studio-sync");
		fs.mkdirSync(dir, { recursive: true });
		const cmd = path.join(dir, "run-daemon.cmd");
		fs.writeFileSync(cmd, `@echo off\r\n:loop\r\n"${node}" "${CLI}" serve >> "${LOG_FILE}" 2>&1\r\ntimeout /t 3 /nobreak > nul\r\ngoto loop\r\n`);
		const vbs = path.join(windowsStartup(), "studio-sync.vbs");
		fs.writeFileSync(vbs, `CreateObject("WScript.Shell").Run """${cmd}""", 0, False\r\n`);
		spawn("wscript.exe", [vbs], { detached: true, stdio: "ignore" }).unref();
		return `Startup launcher ${vbs} (starts at login, restarted if it dies; log ${LOG_FILE})`;
	}
	return "no background service on this OS: run `studio-sync serve` in a terminal";
}

function uninstallService(): void {
	if (process.platform === "darwin") {
		for (const label of [LABEL, ...LEGACY_LABELS]) {
			launchctl("bootout", `${domain()}/${label}`);
			fs.rmSync(plistPath(label), { force: true });
		}
	} else if (process.platform === "win32") {
		fs.rmSync(path.join(windowsStartup(), "studio-sync.vbs"), { force: true });
		spawnSync("wmic", ["process", "where", "commandline like '%run-daemon.cmd%'", "delete"]);
	}
}

export function restartService(pid: number | null): string {
	if (process.platform === "darwin" && fs.existsSync(plistPath(LABEL))) {
		launchctl("kickstart", "-k", `${domain()}/${LABEL}`);
		return "restarted the LaunchAgent";
	}
	if (pid) {
		process.kill(pid);
		return process.platform === "win32" ? "stopped the daemon; its launcher starts it again in 3 s" : `stopped pid ${pid}`;
	}
	return "no daemon running";
}

const skillLink = () => path.join(os.homedir(), ".claude", "skills", "studio-sync");

function installSkill(): string {
	const link = skillLink();
	fs.mkdirSync(path.dirname(link), { recursive: true });
	try {
		const st = fs.lstatSync(link);
		if (!st.isSymbolicLink()) return `${link} exists and is not a link; left alone`;
		fs.unlinkSync(link);
	} catch {}
	fs.symlinkSync(SKILL_DIR, link, process.platform === "win32" ? "junction" : "dir");
	return link;
}

export function install(): string[] {
	const lines: string[] = [];
	const rojo = spawnSync("rojo", ["--version"], { encoding: "utf8", shell: process.platform === "win32" });
	if (rojo.status !== 0) lines.push("WARNING: `rojo` is not on PATH here; the daemon runs it in each project (rokit install there)");
	lines.push(`plugin: ${buildPlugin()} (Studio picks it up on its next start, often live)`);
	lines.push(`service: ${installService()}`);
	lines.push(`Claude skill: ${installSkill()}`);
	try {
		execFileSync(process.platform === "win32" ? "where" : "which", ["studio-sync"], { stdio: "ignore" });
	} catch {
		lines.push("the `studio-sync` command is not on PATH yet: run `npm link` in " + ROOT);
	}
	return lines;
}

export function uninstall(): string[] {
	uninstallService();
	fs.rmSync(PLUGIN_FILE(), { force: true });
	try {
		if (fs.lstatSync(skillLink()).isSymbolicLink()) fs.unlinkSync(skillLink());
	} catch {}
	return ["removed the service, the plugin and the skill link (`npm unlink -g studio-sync` removes the command)"];
}
