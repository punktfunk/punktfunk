// `punktfunk-host plugins …` package operations, run on the vendored bun. The host CLI forwards
// add/remove/list here (crates/punktfunk-host/src/plugins.rs) and the runner-cli exposes them as
// subcommands. Everything a plugin needs to be installed — the plugins dir, the `@punktfunk`
// registry scope in bunfig.toml, and the right bun — is handled here so the operator types one line
// instead of the old create-dir / write-bunfig / `bun add` ritual.
import { createHash } from "node:crypto";
import * as fs from "node:fs";
import * as path from "node:path";
import { configDir } from "./config.js";
import { SDK_VERSION } from "./version.js";

/** The `@punktfunk` package registry (Gitea's npm registry for the `unom` org). */
export const REGISTRY = "https://git.unom.io/api/packages/unom/npm/";

/** Where plugin packages install: `<config_dir>/plugins` (matches runner.ts discovery). */
export const pluginsDirDefault = (): string => path.join(configDir(), "plugins");

export interface ResolveOptions {
	/**
	 * Allow names that resolve on the PUBLIC npm registry (unscoped `punktfunk-plugin-*`, foreign
	 * scopes, arbitrary paths). Off by default: only the `@punktfunk` scope — pinned to the Gitea
	 * registry by [`ensureBunfig`] — installs without it, so a typo or a squatted look-alike
	 * package can't silently pull operator-privileged code from npmjs.org (the CLI flag is
	 * `--allow-public-registry`).
	 */
	allowPublicRegistry?: boolean;
}

/**
 * Resolve a friendly plugin name to its npm package. A bare first-party name maps into the
 * `@punktfunk` scope (`playnite` → `@punktfunk/plugin-playnite`, `rom-manager` →
 * `@punktfunk/plugin-rom-manager`); an `@punktfunk/…` name is used verbatim. Anything else —
 * the unscoped `punktfunk-plugin-…` convention, foreign scopes, registry paths — resolves on
 * the public registry and is refused unless [`ResolveOptions.allowPublicRegistry`] is set.
 */
export const resolvePackage = (
	name: string,
	opts: ResolveOptions = {},
): string => {
	const n = name.trim();
	if (!n) throw new Error("empty plugin name");
	if (!n.startsWith("@") && !n.includes("/") && !n.startsWith("punktfunk-plugin-")) {
		return `@punktfunk/plugin-${n}`; // bare first-party name
	}
	if (n.startsWith("@punktfunk/")) return n; // first-party scope, pinned to our registry
	if (!opts.allowPublicRegistry) {
		throw new Error(
			`'${n}' would install from the PUBLIC npm registry, not Punktfunk's. Plugins run ` +
				"with operator privileges - install only code you trust. If you mean it, re-run " +
				"with --allow-public-registry.",
		);
	}
	return n;
};

/** Does this resolved package name install from Punktfunk's own (Gitea) registry? */
const isFirstParty = (pkg: string): boolean => pkg.startsWith("@punktfunk/");

/**
 * Create the plugins dir (and parents) if needed, and make it bun's install ROOT. On Windows the
 * ACL lockdown is the host's job.
 *
 * The `package.json` is load-bearing, not decoration: `bun add` installs into the nearest ancestor
 * `package.json`, not into its working directory. Without one here, a stray `~/package.json` — one
 * old `bun add`/`npm init` in a home dir — silently captures every plugin install. bun reports
 * success and exits 0, the packages land in that tree, and the plugins dir stays empty (reproduced
 * on-glass 2026-07-31; it presented as a plugin store that installs nothing).
 *
 * Only seeds a tree with no `node_modules`. A dir with packages but no `package.json` is
 * hand-assembled or an older layout, and both this module's [`listInstalled`] and the host's
 * installed-package scan fall back to the naming convention there; an empty `dependencies` would
 * make the host report every plugin already installed as gone.
 */
export const ensurePluginsDir = (dir = pluginsDirDefault()): string => {
	fs.mkdirSync(dir, { recursive: true });
	const manifest = path.join(dir, "package.json");
	if (!fs.existsSync(manifest) && !fs.existsSync(path.join(dir, "node_modules"))) {
		fs.writeFileSync(manifest, '{\n  "name": "punktfunk-plugins",\n  "private": true\n}\n');
	}
	return dir;
};

/**
 * Ensure `<dir>/bunfig.toml` maps every scope we need to its registry, so `bun add` resolves
 * plugins from the right place. `@punktfunk` → Punktfunk's own registry is always mapped;
 * `extraScopes` adds others — a plugin-store catalog entry carries its own registry, and the scope
 * is what binds a package name to it (design D8, which is why catalog entries must be scoped).
 *
 * Idempotent and non-destructive: a scope already mapped to the same URL is left alone, a scope
 * mapped to a *different* URL is rewritten, and any unrelated bunfig content is preserved.
 */
export const ensureBunfig = (
	dir = pluginsDirDefault(),
	extraScopes: Record<string, string> = {},
): void => {
	const file = path.join(dir, "bunfig.toml");
	const wanted: Record<string, string> = { "@punktfunk": REGISTRY, ...extraScopes };
	let existing = "";
	try {
		existing = fs.readFileSync(file, "utf8");
	} catch {
		// no bunfig yet — write a fresh one below
	}

	let out = existing;
	const missing: string[] = [];
	for (const [scope, url] of Object.entries(wanted)) {
		// Match `"@scope" = "…"` (quoted or bare key) anywhere in the file.
		const line = new RegExp(`^\\s*"?${escapeRe(scope)}"?\\s*=\\s*".*"\\s*$`, "m");
		const replacement = `"${scope}" = "${url}"`;
		if (line.test(out)) {
			const current = out.match(line)?.[0] ?? "";
			if (current.includes(`"${url}"`)) continue; // already correct
			out = out.replace(line, replacement);
		} else {
			missing.push(replacement);
		}
	}
	if (missing.length === 0) {
		if (out !== existing) fs.writeFileSync(file, out);
		return;
	}
	const block = missing.join("\n");
	if (!out.trim()) {
		fs.writeFileSync(file, `[install.scopes]\n${block}\n`);
	} else if (/^\[install\.scopes\][^\n]*$/m.test(out)) {
		// Insert under the existing table header.
		fs.writeFileSync(
			file,
			out.replace(/^\[install\.scopes\][^\n]*$/m, (m) => `${m}\n${block}`),
		);
	} else {
		const sep = out.endsWith("\n") ? "" : "\n";
		fs.writeFileSync(file, `${out}${sep}\n[install.scopes]\n${block}\n`);
	}
};

const escapeRe = (s: string): string => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

export interface PkgOpts extends ResolveOptions {
	/** Plugins dir. Default `<config_dir>/plugins`. */
	dir?: string;
	/** Line sink for progress. Default stdout. */
	log?: (line: string) => void;
	/**
	 * Record the resolved version exactly (`bun add --exact`) instead of a caret range. The plugin
	 * store always sets this: a catalog entry pins one reviewed version, and a caret range in
	 * `package.json` would let a later `bun install` in this tree drift off it.
	 */
	exact?: boolean;
	/** Extra `scope → registry URL` mappings to write into `bunfig.toml` before installing. */
	registries?: Record<string, string>;
}

/** Run `bun add`/`bun remove` in the plugins dir on the current (vendored) bun. */
const runBun = (action: "add" | "remove", pkgs: string[], opts: PkgOpts): void => {
	const dir = opts.dir ?? pluginsDirDefault();
	const log = opts.log ?? ((l: string) => console.log(l));
	ensurePluginsDir(dir);
	if (action === "add") ensureBunfig(dir, opts.registries);
	log(`${action === "add" ? "installing" : "removing"} ${pkgs.join(", ")} in ${dir}`);
	// `process.execPath` is the bun running this file (the vendored one under the package), so a
	// system-wide bun on PATH is not required. Inherit stdio so `bun`'s progress reaches the user.
	const args = [process.execPath, action, ...pkgs];
	if (action === "add") {
		// NEVER run install lifecycle scripts. A plugin is code we chose to run under the runner,
		// where it is supervised and (on Windows) de-privileged; a postinstall script runs
		// immediately, as whoever is installing — which on a console-triggered install is the host
		// service. bun already declines untrusted scripts by default; this makes it explicit and
		// unconditional. A plugin that needs a native build step is a review rejection, not a case
		// to support.
		args.push("--ignore-scripts");
		if (opts.exact) args.push("--exact");
	}
	// Windows: install file COPIES, never bun's default hardlinks. A hardlinked file's canonical
	// path resolves into the installing admin's per-user bun cache
	// (C:\Users\<admin>\.bun\install\cache\…), which the de-privileged LocalService runner cannot
	// traverse — imports die with EPERM even though the plugins-dir DACL grants read (seen live
	// on-glass). copyfile keeps the plugins tree self-contained under %ProgramData%.
	if (action === "add" && process.platform === "win32") {
		args.push("--backend=copyfile");
	}
	const res = Bun.spawnSync(args, {
		cwd: dir,
		stdio: ["inherit", "inherit", "inherit"],
	});
	if (!res.success) {
		throw new Error(`bun ${action} exited ${res.exitCode ?? "?"} — see output above`);
	}
	if (action === "add") stripGroupWrite(dir);
};

/**
 * Clear group/world write on everything under `<dir>/node_modules`. The runner refuses a
 * group-writable entry file (`fileIsSafe`), and a umask of 002 — Ubuntu's default for
 * user-private groups — makes every file bun extracts exactly that. bun hardlinks into
 * its cache, so an entry extracted once under 002 stays 664 on every later install;
 * fixing the mode after the fact is the only cure that covers the cache too.
 */
export const stripGroupWrite = (dir: string): void => {
	if (process.platform === "win32") return;
	const root = path.join(dir, "node_modules");
	let names: string[];
	try {
		names = fs.readdirSync(root, { recursive: true }) as string[];
	} catch {
		return;
	}
	for (const rel of ["", ...names]) {
		const p = path.join(root, rel);
		try {
			const st = fs.lstatSync(p);
			if (st.isSymbolicLink()) continue;
			if (st.mode & 0o022) fs.chmodSync(p, st.mode & ~0o022 & 0o7777);
		} catch {
			// A vanished or foreign entry is not ours to fix; the runner judges the entry file.
		}
	}
};

/** The SDK version installed in a plugins tree, or undefined if it isn't installed at all. */
export const installedSdkVersion = (
	dir = pluginsDirDefault(),
): string | undefined => {
	try {
		const manifest = path.join(
			dir,
			"node_modules",
			"@punktfunk",
			"host",
			"package.json",
		);
		const v = (
			JSON.parse(fs.readFileSync(manifest, "utf8")) as { version?: string }
		).version;
		return typeof v === "string" ? v : undefined;
	} catch {
		return undefined;
	}
};

const REFRESH_FAILED = ".pf-sdk-refresh-failed";

/** What a failed refresh is remembered by: the versions, and the plugin set that pinned them. */
const refreshKey = (dir: string, have: string): string => {
	let manifest = "";
	try {
		manifest = fs.readFileSync(path.join(dir, "package.json"), "utf8");
	} catch {}
	const hash = createHash("sha256").update(manifest).digest("hex").slice(0, 16);
	return `${have}->${SDK_VERSION} ${hash}`;
};

/**
 * Bring the plugins tree's `@punktfunk/host` up to the version THIS runner was built from.
 *
 * **Why this exists.** The SDK is the seam every plugin registers through, but each plugin resolves
 * it from the plugins tree, and `bun.lock` pins it to an exact version with an integrity hash. No
 * user-facing flow re-resolves that pin: installing a plugin, reinstalling it, even updating it to a
 * newer release all leave the SDK where it is, because the plugin's `^0.1.x` range is already
 * satisfied. Measured on 2026-08-08 — publishing `@punktfunk/host@0.1.3` (the release that lets a
 * library scanner register `category`, so it stays out of the console nav) reached **no existing
 * install**, and the only thing that moved it was deleting the lockfile by hand over ssh. Shipping a
 * fix that needs an ssh session is not shipping a fix.
 *
 * The runner is the right owner: it is bundled from this same `sdk/` at the host's release commit
 * (`packaging/arch/PKGBUILD` builds `src/runner-cli.ts` into the punktfunk-scripting package), so
 * `SDK_VERSION` is by construction the SDK that matches the host now on disk. A host upgrade then
 * carries the SDK with it and nobody touches a runner.
 *
 * **Why the whole lockfile.** A targeted `bun add @punktfunk/host@<v>` at the root does NOT work
 * while plugins still declare the SDK in their own `dependencies` (they do, though none import it):
 * bun honours their locked resolution and gives each plugin a private nested copy, which then
 * SHADOWS the root — measured, 5 nested copies. A lockless resolve hoists one copy for everyone,
 * also measured. Once the plugins drop that spurious dependency this can become the targeted form.
 *
 * Safety: the plugins' own versions are pinned exactly in the root `package.json`, so a re-resolve
 * cannot move them; only shared transitive deps float within their declared ranges. The lockfile is
 * backed up first and restored if the install fails, and any failure is logged and swallowed — a
 * dependency refresh must never stop the plugins that are already working from loading. One that
 * failed is not retried until the SDK or the installed plugin set changes.
 */
export const reconcileSharedSdk = (
	dir = pluginsDirDefault(),
	log: (line: string) => void = (l) => console.log(l),
): void => {
	const have = installedSdkVersion(dir);
	// Nothing installed = no plugins yet; the first `bun add` resolves the current SDK on its own.
	if (have === undefined || have === SDK_VERSION) return;
	// A refresh that could not deliver this SDK is not repeated on every start: it runs a
	// lockless network install while every plugin waits. A new SDK or a changed plugin set retries.
	const marker = path.join(dir, REFRESH_FAILED);
	const key = refreshKey(dir, have);
	try {
		if (fs.readFileSync(marker, "utf8") === key) return;
	} catch {}

	const lock = path.join(dir, "bun.lock");
	const backup = `${lock}.pf-bak`;
	log(
		`[plugins] @punktfunk/host ${have} installed, this host ships ${SDK_VERSION} — refreshing`,
	);
	let restore = false;
	try {
		if (fs.existsSync(lock)) {
			fs.copyFileSync(lock, backup);
			fs.rmSync(lock);
			restore = true;
		}
		const res = Bun.spawnSync([process.execPath, "install", "--ignore-scripts"], {
			cwd: dir,
			stdio: ["inherit", "inherit", "inherit"],
		});
		if (!res.success) {
			throw new Error(`bun install exited ${res.exitCode ?? "?"}`);
		}
		stripGroupWrite(dir);
		const now = installedSdkVersion(dir);
		if (now !== SDK_VERSION) {
			// The install "succeeded" and still did not deliver the version — better to sit on the
			// known-good tree than to keep a half-resolved one.
			throw new Error(`still ${now ?? "absent"} after install`);
		}
		restore = false;
		if (fs.existsSync(backup)) fs.rmSync(backup);
		fs.rmSync(marker, { force: true });
		log(`[plugins] @punktfunk/host is now ${SDK_VERSION}`);
	} catch (e) {
		log(
			`[plugins] WARNING: @punktfunk/host refresh (${
				e instanceof Error ? e.message : e
			}) — plugins keep running against ${have}`,
		);
		try {
			fs.writeFileSync(marker, key);
		} catch {}
		if (restore && fs.existsSync(backup)) {
			try {
				fs.copyFileSync(backup, lock);
				fs.rmSync(backup);
			} catch {
				// The backup is still on disk under its own name; say so rather than pretend.
				log(`[plugins] the previous lockfile is at ${backup}`);
			}
		}
	}
};

/** Install one or more plugins by friendly name or package. */
export const addPlugins = (names: string[], opts: PkgOpts = {}): void => {
	const pkgs = names.map((n) => resolvePackage(n, opts));
	const log = opts.log ?? ((l: string) => console.log(l));
	for (const pkg of pkgs.filter((p) => !isFirstParty(p))) {
		log(
			`[plugins] WARNING: ${pkg} installs from the public npm registry - it is not ` +
				"published by Punktfunk. It will run with operator privileges.",
		);
	}
	runBun("add", pkgs, opts);
};

/** Uninstall one or more plugins by friendly name or package. Removal is always safe — a name
 * never gates on the registry it once came from. */
export const removePlugins = (names: string[], opts: PkgOpts = {}): void =>
	runBun(
		"remove",
		names.map((n) => resolvePackage(n, { allowPublicRegistry: true })),
		opts,
	);

export interface InstalledPlugin {
	/** npm package name, e.g. `@punktfunk/plugin-playnite` or `punktfunk-plugin-foo`. */
	pkg: string;
	/** Installed version from the package's package.json, if readable. */
	version?: string;
}

/**
 * Enumerate installed plugin packages under `<dir>/node_modules` — the unscoped convention
 * (`punktfunk-plugin-*`) and **any** scope's `plugin-*` (`@punktfunk/plugin-rom-manager`,
 * `@retro-hub/plugin-x`). Mirrors the discovery in runner.ts so `list` shows exactly what the
 * runner would supervise.
 */
export const listInstalled = (dir = pluginsDirDefault()): InstalledPlugin[] => {
	const modules = path.join(dir, "node_modules");
	const out: InstalledPlugin[] = [];
	const versionOf = (pkgDir: string): string | undefined => {
		try {
			const m = JSON.parse(
				fs.readFileSync(path.join(pkgDir, "package.json"), "utf8"),
			) as { version?: string };
			return m.version;
		} catch {
			return undefined;
		}
	};
	let entries: string[];
	try {
		entries = fs.readdirSync(modules).sort();
	} catch {
		return out; // no plugins installed yet
	}
	for (const entry of entries) {
		if (entry.startsWith("punktfunk-plugin-")) {
			out.push({ pkg: entry, version: versionOf(path.join(modules, entry)) });
		} else if (entry.startsWith("@")) {
			let scoped: string[] = [];
			try {
				scoped = fs.readdirSync(path.join(modules, entry)).sort();
			} catch {
				scoped = [];
			}
			for (const s of scoped) {
				if (s.startsWith("plugin-")) {
					out.push({
						pkg: `${entry}/${s}`,
						version: versionOf(path.join(modules, entry, s)),
					});
				}
			}
		}
	}
	return out;
};
