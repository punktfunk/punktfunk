// Env files the console reads itself, as its own account.
//
// A system unit's `EnvironmentFile=` is read by PID 1 as root, links followed. The door's console
// runs as the door's account, which owns the folder its files live in, so those files are named
// in PUNKTFUNK_ENV_FILES instead (`:`-separated) and read here. Values land as systemd would have
// set them: a file beats the unit's `Environment=`, a later file an earlier one, a missing file
// is skipped.
import { readFileSync } from "node:fs";

const LINE = /^\s*(PUNKTFUNK_[A-Z0-9_]+)\s*=\s*(.*?)\s*$/;

/** The `PUNKTFUNK_*` settings of one env file as the host and the password file write them:
 * `KEY=value` lines, `#` and `;` comments, one layer of matching quotes around a value. */
export function parseEnvFile(text) {
	const out = {};
	for (const line of text.split("\n")) {
		if (/^\s*[#;]/.test(line)) continue;
		const m = LINE.exec(line);
		if (m) out[m[1]] = m[2].replace(/^(['"])([\s\S]*)\1$/, "$2");
	}
	return out;
}

/** Sets what the files `env.PUNKTFUNK_ENV_FILES` names hold into `env`; with `only`, just those
 * keys. Cheap enough to call again for a value that moves while the console runs. */
export function loadEnvFiles(env = process.env, only) {
	for (const file of (env.PUNKTFUNK_ENV_FILES ?? "").split(":")) {
		if (!file) continue;
		let text;
		try {
			text = readFileSync(file, "utf8");
		} catch {
			continue;
		}
		for (const [key, value] of Object.entries(parseEnvFile(text))) {
			if (!only || only.includes(key)) env[key] = value;
		}
	}
}

// On import, which the entry does first: before any module reads a setting.
loadEnvFiles();
