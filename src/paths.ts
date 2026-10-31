// Where studio-sync keeps its state, and the knobs tests override
// (STUDIO_SYNC_HOME, STUDIO_SYNC_PORT) so a second daemon can run beside the real one.

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const home = os.homedir();

export const ROOT = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
export const VERSION: string = JSON.parse(fs.readFileSync(path.join(ROOT, "package.json"), "utf8")).version;

export const PORT = Number(process.env.STUDIO_SYNC_PORT ?? 34999);
export const STATE_DIR = process.env.STUDIO_SYNC_HOME ?? path.join(home, ".config", "studio-sync");
export const PROJECTS_FILE = path.join(STATE_DIR, "projects.json");
export const STAGING_ROOT = path.join(STATE_DIR, "staging");
// Studio-side text that disk replaced, files Studio deleted, the losing side of a conflict
export const BACKUP_ROOT = path.join(STATE_DIR, "backups");
export const LOG_FILE = process.env.STUDIO_SYNC_HOME || process.platform === "win32"
	? path.join(STATE_DIR, "daemon.log")
	: path.join(home, "Library", "Logs", "studio-sync.log");

export const PLUGIN_SOURCE_DIR = path.join(ROOT, "plugin");
export const SKILL_DIR = path.join(ROOT, "skill");

export const LABEL = "com.unsquared.studio-sync";
export const LEGACY_LABELS = ["com.kevin.studio-sync"];

export function pluginDir(): string {
	if (process.platform === "win32") {
		return path.join(process.env.LOCALAPPDATA ?? path.join(home, "AppData", "Local"), "Roblox", "Plugins");
	}
	return path.join(home, "Documents", "Roblox", "Plugins");
}
