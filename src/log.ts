// The daemon log: one file, rolled over at 5 MB so a daemon that runs for months stays small.

import fs from "node:fs";
import path from "node:path";
import { LOG_FILE } from "./paths.ts";

const MAX_BYTES = 5 * 1024 * 1024;
let toFile = false;

export function logToFile(): void {
	fs.mkdirSync(path.dirname(LOG_FILE), { recursive: true });
	try {
		if (fs.statSync(LOG_FILE).size > MAX_BYTES) fs.renameSync(LOG_FILE, LOG_FILE + ".1");
	} catch {}
	toFile = true;
}

export function log(message: string): void {
	const line = `${new Date().toTimeString().slice(0, 8)} ${message}\n`;
	if (toFile) fs.appendFileSync(LOG_FILE, line);
	else process.stdout.write(line);
}
