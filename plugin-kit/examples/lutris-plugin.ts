// A COMPLETE library-scanner plugin, and the template the six first-party ones are cut from.
//
// This is the lutris pilot (design M5/WP5.1) — the smallest of the six, and the one that exercises
// the POSIX local-art path end to end. It lives here as a worked example rather than shipped code:
// each scanner gets its OWN repo (the house pattern), and this is what you copy into a fresh one.
// `package.json`'s `files` is dist + README, so nothing here is published.
//
// The point it proves: everything below the `scan` function is store-specific parsing, and
// everything else — the store claim, the sync engine, launcher entries, `__config`, the console
// registration, the CLI verbs including the parity gate — comes from `defineLibraryPlugin`. That is
// what makes six repos cost nothing in duplication.
//
// Ported from crates/punktfunk-host/src/library/lutris.rs, with two deliberate changes:
//   * art is emitted as `file://` URLs instead of inlined `data:` URLs. The host proxies the bytes,
//     so the reconcile payload stays tiny — inlining covers is what blew the host's 2 MB body limit
//     at 49 titles during the playnite work, and it is exactly why the POSIX art path exists (G4).
//   * the `installed = 1` filter and the untrusted-slug guard are carried over verbatim. The slug
//     comes from Lutris's own database and is interpolated into a path, so the guard is load-bearing.
import * as os from "node:os";
import * as path from "node:path";
import { Effect, Schema } from "effect";
import {
	defineLibraryPlugin,
	fileUrl,
	isFile,
	withReadOnlyDb,
} from "../src/library/index.js";
import type { ProviderEntry } from "../src/wire.js";

const LutrisConfig = Schema.Struct({
	/**
	 * Where `pga.db` lives, when it isn't in one of the standard places. Annotated because the
	 * console's generic settings form derives its label and help text from exactly these.
	 */
	databasePath: Schema.optionalKey(
		Schema.String.annotate({
			title: "Lutris database",
			description:
				"Absolute path to pga.db. Leave empty to find it automatically.",
		}),
	),
});

/** Candidate `pga.db` locations: XDG data dir, the classic path, Flatpak. */
const databaseCandidates = (): string[] => {
	const out: string[] = [];
	const xdg = process.env.XDG_DATA_HOME;
	if (xdg) out.push(path.join(xdg, "lutris/pga.db"));
	const home = os.homedir();
	if (home) {
		out.push(path.join(home, ".local/share/lutris/pga.db"));
		out.push(path.join(home, ".var/app/net.lutris.Lutris/data/lutris/pga.db"));
	}
	return out;
};

const findDatabase = (cfg: { databasePath?: string }): string | undefined =>
	[
		...(cfg.databasePath ? [cfg.databasePath] : []),
		...databaseCandidates(),
	].find(isFile);

/**
 * `<kind>/<slug>.jpg` across the current, legacy-cache and Flatpak Lutris roots.
 *
 * The slug comes verbatim from Lutris's database and is interpolated into a path, so a separator,
 * parent ref or NUL is refused — otherwise a crafted slug is an arbitrary-file-read primitive, and
 * the resulting path would be handed to the host's art proxy to serve (security-review 2026-07-17).
 * Real Lutris slugs are `[a-z0-9-]`.
 */
const artFile = (kind: string, slug: string): string | undefined => {
	if (
		slug === "" ||
		slug.includes("/") ||
		slug.includes("\\") ||
		slug.includes("..") ||
		slug.includes("\0")
	) {
		return undefined;
	}
	const home = os.homedir();
	if (!home) return undefined;
	const roots = [
		path.join(home, ".local/share/lutris"),
		path.join(home, ".cache/lutris"),
		path.join(home, ".var/app/net.lutris.Lutris/data/lutris"),
		path.join(home, ".var/app/net.lutris.Lutris/cache/lutris"),
	];
	for (const root of roots) {
		const p = path.join(root, kind, `${slug}.jpg`);
		if (isFile(p)) return p;
	}
	return undefined;
};

interface GameRow {
	id: number;
	slug: string | null;
	name: string;
	directory: string | null;
}

const plugin = defineLibraryPlugin({
	// One string: plugin id, provider id, store claim, and the id of the built-in scanner this
	// replaces. That identity chain is what keeps entry ids, GameStream app ids and the operator's
	// existing enable/disable state intact across the migration.
	name: "lutris",
	configSchema: LutrisConfig,

	detect: (cfg) => Effect.sync(() => findDatabase(cfg) !== undefined),

	scan: (cfg) =>
		Effect.sync(() => {
			const db = findDatabase(cfg);
			if (!db) return [];
			// Read-only + immutable: a running Lutris holding the file can neither block us nor be
			// disturbed by us.
			const rows =
				withReadOnlyDb(db, (h) =>
					// `directory` is our only detect signal but is not load-bearing for the library, so
					// a schema without it must not cost the whole source — the helper answers [] on a
					// bad query, and the fallback keeps the titles.
					h.query<GameRow>(
						"SELECT id, slug, name, directory FROM games " +
							"WHERE installed = 1 AND name IS NOT NULL AND name <> '' " +
							"ORDER BY name COLLATE NOCASE",
					),
				) ?? [];
			const usable =
				rows.length > 0
					? rows
					: (withReadOnlyDb(db, (h) =>
							h.query<GameRow>(
								"SELECT id, slug, name, NULL AS directory FROM games " +
									"WHERE installed = 1 AND name IS NOT NULL AND name <> '' " +
									"ORDER BY name COLLATE NOCASE",
							),
						) ?? []);

			return usable.map((row): ProviderEntry => {
				const portrait = row.slug ? artFile("coverart", row.slug) : undefined;
				const header = row.slug ? artFile("banners", row.slug) : undefined;
				const dir = row.directory?.trim();
				return {
					// The host composes `lutris:<external_id>` — byte-identical to what the built-in
					// scanner produced, which the parity gate checks.
					external_id: String(row.id),
					title: row.name,
					launch: { kind: "lutris_id", value: String(row.id) },
					art: {
						...(portrait ? { portrait: fileUrl(portrait) } : {}),
						...(header ? { header: fileUrl(header) } : {}),
					},
					// Lutris stamps no per-game env marker worth relying on, so the install dir is the
					// whole recipe; a game with none (an emulator entry pointing at a bare ROM) stays
					// untracked, exactly as it did in-host.
					...(dir ? { detect: { install_dir: dir } } : {}),
					platform: "PC",
				};
			});
		}),

	// Re-scan when Lutris writes: installing a game touches the database, and downloading art
	// touches the cover directories.
	watchDirs: (cfg) => {
		const db = findDatabase(cfg);
		return db ? [path.dirname(db)] : [];
	},
});

export default plugin.def;
