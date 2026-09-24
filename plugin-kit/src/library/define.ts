// `defineLibraryPlugin` — the shared framework behind every library-scanner plugin (design D10).
//
// The point of this module is that a first-party scanner should be **its parsers and a scan
// function**, ~200–400 lines, and nothing else. Everything a scanner needs beyond that is identical
// across all six of them and lives here: claiming the store, reconciling through the sync engine,
// appending launcher entries, serving `__config` so the console renders settings without the plugin
// shipping an SPA, registering under `category: "library"` so it stays out of the nav, and the
// standard CLI verbs.

import * as fs from "node:fs";
import type { PluginDef } from "@punktfunk/host";
import { Duration, Effect, type Schema, Stream } from "effect";
import {
	type AccessRequestOutcome,
	requestAccess,
	unreachable,
} from "../access.js";
import { type CliCommand, runPluginCli } from "../cli.js";
import { type ConfigService, makeConfigService } from "../config.js";
import { HostClient, type PluginInfo } from "../host-client.js";
import { ProviderClient, type ProviderClientService } from "../reconcile.js";
import { definePluginKit, type PluginKitDef } from "../runtime.js";
import {
	DEFAULT_FS_CHANGE_MIN_INTERVAL,
	makeSyncEngine,
} from "../sync-engine.js";
import { serveUi } from "../ui-server.js";
import type { ProviderEntry } from "../wire.js";
import {
	diffParity,
	formatParityReport,
	fromHostEntry,
	fromProviderEntry,
	type HostGameEntry,
} from "./parity.js";

/** What a scan produced — the status surface and the CLI's `scan` verb both render this. */
export interface ScanReport {
	readonly entries: number;
	readonly launchers: number;
	/** False when the launcher isn't installed here — the library is legitimately empty. */
	readonly present: boolean;
}

export interface LibraryPluginDef<S extends Schema.Top> {
	/**
	 * The plugin id. **This one string is also the provider id, the store claim, and the id of the
	 * built-in scanner this plugin replaces.** That identity chain is what makes the migration
	 * invisible: entry ids stay `<name>:<external_id>`, GameStream app ids and client art caches
	 * stay valid, and the operator's existing enable/disable state carries over untouched.
	 */
	readonly name: string;
	readonly version?: string;
	/**
	 * The store to claim (design D2). Defaults to {@link name} and should almost never differ — see
	 * the identity note above. Pass `null` to opt out of claiming entirely, which makes this an
	 * ordinary unclaimed provider whose entries surface as `custom:`.
	 */
	readonly store?: string | null;
	/** The operator-facing config schema. Drives `__config` and every callback's argument. */
	readonly configSchema: S;
	/**
	 * Is this launcher present on the host at all? Surfaces in the CLI's `detect` verb, and lets the
	 * plugin report "not installed" rather than silently syncing an empty library.
	 */
	readonly detect: (cfg: S["Type"]) => Effect.Effect<boolean>;
	/** Enumerate the launcher's installed titles — the only real per-store code. */
	readonly scan: (
		cfg: S["Type"],
	) => Effect.Effect<ReadonlyArray<ProviderEntry>>;
	/**
	 * Entries that open the LAUNCHER itself (design D4) — Steam Big Picture, Heroic, … Appended to
	 * every reconcile, so toggling one in config takes effect on the next sync. Emit them with
	 * `role: "launcher"`; the kit does not stamp it for you, because a plugin may legitimately want
	 * an entry that opens a launcher but still lists as an ordinary game.
	 *
	 * Give each one an `icon` too — the token for its brand mark (`ProviderEntry.icon`). Without it
	 * the tile falls back to the launcher's name on a flat accent face, which is legible but is the
	 * blandest thing in the grid.
	 */
	readonly launchers?: (cfg: S["Type"]) => ReadonlyArray<ProviderEntry>;
	/** Launcher data dirs to watch, so a newly installed game appears without waiting for a poll. */
	readonly watchDirs?: (cfg: S["Type"]) => ReadonlyArray<string>;
	/** Folders the launcher lists dynamically; unusable ones are requested before each scan. */
	readonly wants?: (cfg: S["Type"]) => ReadonlyArray<string>;
	/** How often to re-scan regardless of watches. Default `Duration.minutes(15)`. */
	readonly pollInterval?: Duration.Duration;
	/** Debounce on filesystem events. Default `Duration.seconds(3)`. */
	readonly debounce?: Duration.Duration;
	/**
	 * Floor between two filesystem-triggered syncs, on top of the debounce: a debounce collapses a
	 * burst, this caps the rate under sustained churn (a launcher writing to its dirs while a game
	 * runs). Changes inside the interval coalesce into one trailing sync. Default
	 * `Duration.seconds(30)`.
	 */
	readonly minInterval?: Duration.Duration;
	/** Display title (the console's sources row falls back to the scanner label). Defaults to `name`. */
	readonly title?: string;
	/** Extra CLI verbs beyond the standard `detect` / `scan` / `uninstall` set. */
	readonly commands?: Record<string, CliCommand<never>>;
}

/** `--flag value` from an argv slice, or undefined. */
const flagValue = (
	argv: ReadonlyArray<string>,
	flag: string,
): string | undefined => {
	const i = argv.indexOf(flag);
	return i >= 0 && i + 1 < argv.length ? argv[i + 1] : undefined;
};

/** The pieces a library plugin package wires into its entry points. */
export interface LibraryPlugin {
	/** The runner-discovered default export (`export default plugin.def`). */
	readonly def: PluginDef;
	/** The CLI entry (`await plugin.cli()` from the package's bin). */
	readonly cli: (argv?: ReadonlyArray<string>) => Promise<void>;
}

/** One line per folder the host answered; a launcher that is simply not installed stays quiet. */
const logOutcomes = (outcomes: ReadonlyArray<AccessRequestOutcome>) =>
	Effect.forEach(
		outcomes,
		({ path, outcome }) => {
			if (outcome === "pending")
				return Effect.logInfo(`asked the operator for ${path}`);
			if (outcome === "denied")
				return Effect.logInfo(`the operator did not allow ${path}`);
			if (outcome === "granted")
				return Effect.logWarning(
					`${path} is allowed but not visible to this plugin — a folder that appeared after the runner started needs a runner restart`,
				);
			if (outcome === "refused:not_directory") return Effect.void;
			return Effect.logInfo(
				`the host won't offer ${path} (${outcome.replace(/^refused:/, "")})`,
			);
		},
		{ discard: true },
	);

/**
 * Ask for the wanted folders the plugin can't reach, once per distinct set, then scan. A request
 * the host never answered is asked again on the next scan; it never fails the scan.
 */
const accessAwareCompute = <Cfg, A, E, R>(
	wants: ((cfg: Cfg) => ReadonlyArray<string>) | undefined,
	title: string,
	load: Effect.Effect<Cfg, E, R>,
	compute: (cfg: Cfg) => Effect.Effect<A, E, R>,
): (() => Effect.Effect<A, E, R | HostClient>) => {
	let lastAsked: string | undefined;
	return () =>
		load.pipe(
			Effect.flatMap((cfg) => {
				const dirs = unreachable(wants?.(cfg) ?? []).sort();
				const key = JSON.stringify(dirs);
				if (key === lastAsked) return compute(cfg);
				if (dirs.length === 0) {
					lastAsked = key;
					return compute(cfg);
				}
				return requestAccess(dirs, title).pipe(
					Effect.tap((outcomes) => {
						if (outcomes.length === dirs.length) lastAsked = key;
						return logOutcomes(outcomes);
					}),
					Effect.andThen(compute(cfg)),
				);
			}),
		);
};

/** Build a library plugin whose scans also reconcile any dynamic folder requests. */
export const defineLibraryPlugin = <S extends Schema.Top>(
	def: LibraryPluginDef<S>,
): LibraryPlugin => {
	const store = def.store === null ? undefined : (def.store ?? def.name);
	const poll = def.pollInterval ?? Duration.minutes(15);
	const debounce = def.debounce ?? Duration.seconds(3);
	const minInterval = def.minInterval ?? DEFAULT_FS_CHANGE_MIN_INTERVAL;

	/** The config service, built fresh wherever it is needed (it only requires `PluginInfo`). */
	const config: Effect.Effect<
		ConfigService<S>,
		never,
		PluginInfo
	> = makeConfigService({ schema: def.configSchema });

	/** Scan + launcher entries, in the order they should reach the host. */
	const computeEntries = (
		cfg: S["Type"],
	): Effect.Effect<{
		readonly entries: ReadonlyArray<ProviderEntry>;
		readonly report: ScanReport;
	}> =>
		Effect.gen(function* () {
			const present = yield* def.detect(cfg);
			// A launcher that isn't installed contributes NOTHING — not even its launcher entries. A
			// "Steam Big Picture" tile on a box without Steam would only fail to launch.
			if (!present) {
				return {
					entries: [] as ReadonlyArray<ProviderEntry>,
					report: { entries: 0, launchers: 0, present: false } as const,
				};
			}
			const scanned = yield* def.scan(cfg);
			const launchers = def.launchers?.(cfg) ?? [];
			return {
				entries: [...scanned, ...launchers],
				report: {
					entries: scanned.length,
					launchers: launchers.length,
					present: true,
				} as const,
			};
		});

	/**
	 * Push one entry set to the host under the store claim, warning **once** if the host is too old
	 * to honour it.
	 *
	 * This degradation is worth the code: a pre-M2 host ignores `?store=` silently, and the only
	 * symptom would be this plugin's titles appearing as unbadged `custom:` entries *beside* the
	 * built-in scanner's identical ones — a confusing double-listing with no error anywhere.
	 * Checking the echoed entries turns that into one actionable log line.
	 */
	const applyEntries =
		(provider: ProviderClientService, state: { warned: boolean }) =>
		(entries: ReadonlyArray<ProviderEntry>): Effect.Effect<void, unknown> =>
			provider.reconcile(def.name, entries, store).pipe(
				Effect.tap((echoed) => {
					if (!store || state.warned || echoed.length === 0) return Effect.void;
					if (echoed.some((e) => e.store === store)) return Effect.void;
					state.warned = true;
					return Effect.logWarning(
						`host is too old for store claims: this source's games will appear as custom ` +
							`entries and the host's own "${store}" scanner is not suppressed, so titles ` +
							`may be listed twice. Updating the host resolves it.`,
					);
				}),
				Effect.asVoid,
			);

	const main = Effect.gen(function* () {
		const cfgService = yield* config;
		const provider = yield* ProviderClient;
		const state = { warned: false };
		const compute = accessAwareCompute(
			def.wants,
			def.title ?? def.name,
			cfgService.load,
			computeEntries,
		);

		const engine = yield* makeSyncEngine<
			ScanReport,
			ReadonlyArray<ProviderEntry>,
			HostClient
		>({
			compute,
			apply: applyEntries(provider, state),
			// The host IS the state: a full-replace reconcile is idempotent, so there is nothing to
			// persist between runs. Reporting no previous fingerprint means the first sync after a
			// restart always pushes, which is exactly what we want (the host may have been reinstalled
			// underneath us).
			lastSync: { get: Effect.succeed(undefined), set: () => Effect.void },
			settings: cfgService.load.pipe(
				Effect.map((cfg) => def.watchDirs?.(cfg) ?? []),
				// A config file that won't decode must not stop the poll loop: fall back to no watch
				// dirs, keep syncing on the timer, and let the operator see the parse error in the
				// settings drawer (`GET /__config` reports it).
				Effect.catch(() => Effect.succeed([] as ReadonlyArray<string>)),
				Effect.map((watchDirs) => ({
					pollInterval: poll,
					watch: true,
					debounce,
					minInterval,
					watchDirs,
				})),
			),
		});

		// The UI server exists ONLY to serve `__config` (and the SDK's `__health`): no `staticDir`,
		// no API. That is the whole "settings without an SPA" story (design D7, closing G8), and the
		// `library` category is what keeps six installed scanners out of the console's sidebar.
		yield* serveUi({
			title: def.title ?? def.name,
			category: "library",
			config: { schema: def.configSchema, service: cfgService },
		});

		yield* engine.start;
		// A saved settings change is exactly when a user expects the library to update — and it may
		// have changed `watchDirs`, so re-read settings rather than just re-syncing.
		yield* Effect.forkScoped(
			Stream.runForEach(cfgService.changes, () => engine.reconfigure),
		);
		yield* Effect.never;
	});

	const kitDef: PluginKitDef<never, ProviderClient> = {
		name: def.name,
		...(def.version !== undefined ? { version: def.version } : {}),
		layer: ProviderClient.layer,
		main: main as Effect.Effect<
			void,
			never,
			ProviderClient | HostClient | PluginInfo | never
		>,
	};

	const standardCommands: Record<string, CliCommand<ProviderClient>> = {
		detect: {
			summary: "report whether this launcher is installed on the host",
			// Offline on purpose: "is Steam here?" must be answerable without a running host.
			offline: true,
			run: () =>
				Effect.gen(function* () {
					const cfg = yield* (yield* config).load;
					console.log((yield* def.detect(cfg)) ? "present" : "absent");
				}),
		},
		scan: {
			summary:
				"scan and print what WOULD be synced (--preview for the JSON entries)",
			// Also offline: the point is to debug a scanner against real launcher files without
			// touching the host's library.
			offline: true,
			run: (argv) =>
				Effect.gen(function* () {
					const cfg = yield* (yield* config).load;
					const { entries, report } = yield* computeEntries(cfg);
					if (argv.includes("--preview")) {
						console.log(JSON.stringify(entries, null, 2));
					} else {
						console.log(
							`${report.present ? "present" : "absent"}: ${report.entries} games, ` +
								`${report.launchers} launcher entries`,
						);
					}
				}),
		},
		parity: {
			summary:
				"prove this plugin reproduces the built-in scanner (--snapshot <f> | --compare <f>)",
			// `--compare` is offline (it runs THIS plugin's scan); `--snapshot` needs the host. The
			// dispatcher decides per invocation below, so the verb is registered as online and the
			// snapshot path is the one that actually uses the client.
			run: (argv) =>
				Effect.gen(function* () {
					const snapshot = flagValue(argv, "--snapshot");
					const compare = flagValue(argv, "--compare");
					if (!snapshot && !compare) {
						console.error(
							"usage: parity --snapshot <file>   (capture the host's CURRENT library for this store)\n" +
								"       parity --compare  <file>   (diff this plugin's scan against that capture)",
						);
						process.exitCode = 2;
						return;
					}
					if (snapshot) {
						// The baseline: what the host reports for THIS store while its built-in scanner
						// is still the thing producing it. Capture before installing the plugin.
						const host = yield* HostClient;
						const body = yield* host.request("GET", "/library");
						const mine = (Array.isArray(body) ? (body as HostGameEntry[]) : [])
							.filter((e) => e.store === (store ?? def.name))
							.map(fromHostEntry)
							.sort((a, b) => a.id.localeCompare(b.id));
						yield* Effect.sync(() =>
							fs.writeFileSync(snapshot, `${JSON.stringify(mine, null, 2)}\n`),
						);
						console.log(
							`captured ${mine.length} "${store ?? def.name}" entries to ${snapshot}`,
						);
						return;
					}
					const baseline = yield* Effect.try({
						try: () =>
							JSON.parse(
								fs.readFileSync(compare as string, "utf8"),
							) as ReturnType<typeof fromHostEntry>[],
						catch: (cause) => new Error(`read ${compare}: ${cause}`),
					});
					const cfg = yield* (yield* config).load;
					const { entries } = yield* computeEntries(cfg);
					const produced = entries.map((e) =>
						fromProviderEntry(store ?? def.name, e),
					);
					const report = diffParity(baseline, produced);
					console.log(formatParityReport(report));
					// A non-zero exit is what makes this usable as a release gate rather than a report
					// somebody skims.
					if (!report.ok) process.exitCode = 1;
				}),
		},
		uninstall: {
			summary:
				"remove this source's games from the host and release its store claim",
			run: () =>
				Effect.gen(function* () {
					const provider = yield* ProviderClient;
					// The empty reconcile clears the entries; DELETE is what releases the CLAIM — and
					// releasing is what brings the host's own built-in scanner straight back.
					yield* provider.reconcile(def.name, [], undefined);
					yield* provider.remove(def.name);
					console.log(`${def.name}: entries removed, store claim released`);
				}),
		},
	};

	return {
		def: definePluginKit(kitDef),
		cli: (argv) =>
			runPluginCli({
				def: kitDef,
				commands: {
					...standardCommands,
					...(def.commands ?? {}),
				} as Record<string, CliCommand<ProviderClient>>,
				...(argv !== undefined ? { argv } : {}),
			}),
	};
};
