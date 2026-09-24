// Where Steam lives on this host, and which `steamapps` dirs hold installed titles.
//
// Ported from the host scanner (steam.rs `steam_roots` / `steam_library_dirs`) with one deliberate
// addition and one deliberate exclusion, both about the Windows runner's account:
//
//   * ADDED: HKLM `WOW6432Node\Valve\Steam\InstallPath`, so a non-default Steam install dir is
//     found. The host scanner never covered this (it relied on an explorer.exe protocol fallback at
//     launch time), but a plugin that can't find the root finds no games at all.
//   * EXCLUDED: HKCU `Software\Valve\Steam`. The runner is LocalService, whose HKCU is its own empty
//     hive, not the operator's — reading it would look like "Steam isn't installed".
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { isDir, listDir, readTextCapped } from "./fs.js";
import { regQueryValue } from "./registry.js";
import { vdfPaths } from "./vdf.js";

/** Canonicalize-ish: resolve and drop a trailing separator so dedup is reliable. */
const norm = (p: string): string => path.resolve(p);

/**
 * A root's real path, so `~/.steam/steam` and `~/.steam/root` dedupe into the
 * `~/.local/share/Steam` they link to; a path that doesn't resolve stays as spelled.
 */
const realRoot = (p: string): string => {
	try {
		return fs.realpathSync(p);
	} catch {
		return norm(p);
	}
};

/**
 * Candidate Steam roots that actually exist (have a `steamapps` dir), deduped.
 *
 * A "root" is the Steam install itself — `userdata/`, `appcache/` and the first `steamapps/` live
 * under it. Extra library folders on other drives are NOT roots; see {@link steamLibraryDirs}.
 */
export const steamRoots = (): string[] => {
	const candidates: string[] = [];
	if (process.platform === "win32") {
		for (const v of ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"]) {
			const pf = process.env[v];
			if (pf) candidates.push(path.join(pf, "Steam"));
		}
		// The registry install path — covers a Steam installed somewhere other than Program Files.
		for (const key of [
			"HKLM\\SOFTWARE\\WOW6432Node\\Valve\\Steam",
			"HKLM\\SOFTWARE\\Valve\\Steam",
		]) {
			const p = regQueryValue(key, "InstallPath");
			if (p) candidates.push(p);
		}
	} else {
		const home = os.homedir();
		if (home) {
			candidates.push(
				path.join(home, ".local/share/Steam"),
				path.join(home, ".steam/steam"),
				path.join(home, ".steam/root"),
				// Flatpak Steam
				path.join(home, ".var/app/com.valvesoftware.Steam/.local/share/Steam"),
			);
		}
	}
	const seen = new Set<string>();
	const roots: string[] = [];
	for (const c of candidates) {
		const n = realRoot(c);
		if (!seen.has(n) && isDir(path.join(n, "steamapps"))) {
			seen.add(n);
			roots.push(n);
		}
	}
	return roots;
};

/** Steam library roots as listed by Steam, including paths this process cannot stat yet. */
export const steamListedLibraries = (roots = steamRoots()): string[] => {
	const seen = new Set<string>();
	const listed: string[] = [];
	const push = (value: string) => {
		const library = norm(value);
		if (seen.has(library)) return;
		seen.add(library);
		listed.push(library);
	};
	for (const root of roots) {
		const text = readTextCapped(
			path.join(root, "steamapps", "libraryfolders.vdf"),
		);
		if (text !== undefined) for (const library of vdfPaths(text)) push(library);
		push(root);
	}
	return listed;
};

/** Existing `steamapps` directories from every root Steam lists. */
export const steamLibraryDirs = (roots = steamRoots()): string[] =>
	steamListedLibraries(roots)
		.map((library) => path.join(library, "steamapps"))
		.filter(isDir);

/**
 * Every `userdata/<accountId>/config` dir across all roots — one per Steam account that has signed
 * in on this host. `shortcuts.vdf` and the `grid/` art overrides live here.
 */
export const steamUserConfigDirs = (roots = steamRoots()): string[] => {
	const out: string[] = [];
	for (const root of roots) {
		const userdata = path.join(root, "userdata");
		for (const acct of listDir(userdata)) {
			const cfg = path.join(userdata, acct, "config");
			if (isDir(cfg)) out.push(cfg);
		}
	}
	return out;
};
