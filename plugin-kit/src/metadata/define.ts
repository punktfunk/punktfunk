// `defineMetadataPlugin` — the framework behind every Art & Metadata source (planning
// `design/metadata-sources.md` D10). A source is its matching and fetching. The kit reads the
// library, picks the entries worth a lookup, caches verdicts, backs off when told to, pushes the
// result, and serves the console's Choose dialog under `/__metadata/`.
import type { PluginDef } from "@punktfunk/host";
import {
	Cause,
	Data,
	Duration,
	Effect,
	Layer,
	Queue,
	Ref,
	Schema,
	Stream,
} from "effect";
import { makeCacheStore } from "../cache-store.js";
import { type CliCommand, runPluginCli } from "../cli.js";
import { type ConfigService, makeConfigService } from "../config.js";
import type { UiServeError } from "../errors.js";
import { HostClient, type PluginInfo } from "../host-client.js";
import { definePluginKit, type PluginKitDef } from "../runtime.js";
import { type LastSync, makeSyncEngine } from "../sync-engine.js";
import { serveUi, validEntryId } from "../ui-server.js";
import { ART_KINDS, type ArtKind, type MetadataEntry } from "../wire.js";
import {
	type Found,
	identityOf,
	isFresh,
	isHttpUrl,
	type LibraryEntry,
	type Match,
	type Offers,
	ownView,
	toRow,
	Verdict,
	wanted,
} from "./rules.js";

/** One game a search offers for "wrong game?". `key` becomes the operator's pin. */
export interface Candidate {
	readonly key: string;
	readonly label: string;
	readonly thumb?: string | null;
}

/** One image a source offers for a slot of a match. */
export interface Image {
	readonly url: string;
	readonly thumb?: string;
	readonly label?: string;
	readonly width?: number;
	readonly height?: number;
}

/** Fail a lookup with this when the source asks to slow down: the round stops and retries later. */
export class SourceRateLimited extends Data.TaggedError("SourceRateLimited")<{
	readonly retryAfterMs?: number;
}> {}

/** Fail a lookup with this when the source refuses the credentials: the round stops, status says why. */
export class SourceUnauthorized extends Data.TaggedError("SourceUnauthorized")<{
	readonly reason: string;
}> {}

export interface MetadataPluginDef<S extends Schema.Top> {
	/** The plugin id; also the source id the host lists and the console orders. */
	readonly name: string;
	readonly version?: string;
	/** Console title. Defaults to `name`. */
	readonly title?: string;
	readonly configSchema: S;
	/** `exact` matches only by `ids`; `search` falls back to a title search. Sets the default order. */
	readonly matching: "exact" | "search";
	readonly offers: Offers;
	/** Bump to re-resolve every cached verdict after a matching change. Default 1. */
	readonly matcherVersion?: number;
	/** Why the source can't run with this config (no API key), in words for the console. */
	readonly notReady?: (cfg: S["Type"]) => string | undefined;
	/** Find the entry in the source's catalog. `pin` is the operator's chosen key, if any. */
	readonly match: (
		entry: LibraryEntry,
		cfg: S["Type"],
		pin: string | undefined,
	) => Effect.Effect<Match | null, unknown>;
	/** Everything the source has for a match. */
	readonly fetch: (
		match: Match,
		entry: LibraryEntry,
		cfg: S["Type"],
	) => Effect.Effect<Found, unknown>;
	/** Games for a search term, for "wrong game?". Omit on an exact source. */
	readonly search?: (
		term: string,
		entry: LibraryEntry,
		cfg: S["Type"],
	) => Effect.Effect<ReadonlyArray<Candidate>, unknown>;
	/** Every image for one slot of a match. Default: the one `fetch` found. */
	readonly images?: (
		match: Match,
		kind: ArtKind,
		cfg: S["Type"],
	) => Effect.Effect<ReadonlyArray<Image>, unknown>;
	/** Re-check the whole library this often. Default 6 hours. */
	readonly pollInterval?: Duration.Duration;
	/** Lookups in flight at once. Default 4. */
	readonly concurrency?: number;
	readonly commands?: Record<string, CliCommand<never>>;
}

/** `GET /__metadata/status`: the console's status line for this source. */
export interface MetadataStatus {
	readonly ready: boolean;
	/** Why the source is not filling anything, in words. */
	readonly reason?: string;
	/** Entries worth a lookup, and how many of them this source has something for. */
	readonly wanted: number;
	readonly found: number;
	readonly lastRun?: number;
	readonly searchable: boolean;
}

export interface MetadataPlugin {
	readonly def: PluginDef;
	readonly cli: (argv?: ReadonlyArray<string>) => Promise<void>;
}

interface RoundReport {
	readonly wanted: number;
	readonly found: number;
	readonly lookedUp: number;
	readonly failed: number;
}

const Verdicts = Schema.Record(Schema.String, Verdict);
const Pins = Schema.Record(Schema.String, Schema.String);

const describe = (e: unknown): string =>
	e instanceof Error ? e.message : typeof e === "string" ? e : String(e);

const json = (body: unknown, status = 200): Response =>
	Response.json(body, { status });

type Env = HostClient | PluginInfo;

/** Build an Art & Metadata source from its matching and fetching. */
export const defineMetadataPlugin = <S extends Schema.Top>(
	def: MetadataPluginDef<S>,
): MetadataPlugin => {
	const matcher = def.matcherVersion ?? 1;
	const concurrency = def.concurrency ?? 4;
	const poll = def.pollInterval ?? Duration.hours(6);
	const config: Effect.Effect<
		ConfigService<S>,
		never,
		PluginInfo
	> = makeConfigService({ schema: def.configSchema });

	const listLibrary = Effect.gen(function* () {
		const body = yield* (yield* HostClient).request("GET", "/library");
		return (Array.isArray(body) ? body : []) as ReadonlyArray<LibraryEntry>;
	});

	/** The operator's "Use for every game" switch for this source. */
	const readReplace = Effect.gen(function* () {
		const body = yield* (yield* HostClient).request("GET", "/library/metadata");
		const mine = (Array.isArray(body) ? body : []).find(
			(s: { id?: unknown }) => s?.id === def.name,
		) as { replace?: unknown } | undefined;
		return mine?.replace === true;
	}).pipe(Effect.orElseSucceed(() => false));

	/** Match and fetch one entry, keeping only what the source offers. */
	const resolve = (
		e: LibraryEntry,
		cfg: S["Type"],
		pin: string | undefined,
		now: number,
	): Effect.Effect<Verdict, unknown> =>
		Effect.gen(function* () {
			const own = ownView(e);
			const match = yield* def.match(own, cfg, pin);
			const found = match ? yield* def.fetch(match, own, cfg) : undefined;
			const row = found ? toRow(e.id, found, def.offers) : undefined;
			return {
				v: matcher,
				identity: identityOf(e, pin),
				at: now,
				match: match ? { key: match.key, label: match.label } : null,
				found: row
					? {
							...(row.art ? { art: row.art } : {}),
							...(row.meta ? { meta: row.meta } : {}),
						}
					: null,
			};
		});

	const main = Effect.gen(function* () {
		const cfgService = yield* config;
		const host = yield* HostClient;
		const verdicts = yield* makeCacheStore({
			schema: Verdicts,
			empty: {},
			fileName: "verdicts.json",
		});
		const pins = yield* makeCacheStore({
			schema: Pins,
			empty: {},
			fileName: "pins.json",
		});
		const status = yield* Ref.make<MetadataStatus>({
			ready: false,
			wanted: 0,
			found: 0,
			searchable: def.search !== undefined,
		});
		const kick = yield* Queue.sliding<void>(1);
		const lastSync = yield* Ref.make<LastSync | undefined>(undefined);
		const ctx = yield* Effect.context<Env>();
		const run = <A>(eff: Effect.Effect<A, unknown, Env>): Promise<A> =>
			Effect.runPromise(Effect.provide(eff, ctx));

		const round = Effect.gen(function* () {
			const cfg = yield* cfgService.load;
			const notReady = def.notReady?.(cfg);
			const searchable = def.search !== undefined;
			if (notReady !== undefined) {
				yield* Ref.set(status, {
					ready: false,
					reason: notReady,
					wanted: 0,
					found: 0,
					lastRun: Date.now(),
					searchable,
				});
				return {
					entries: [] as ReadonlyArray<MetadataEntry>,
					report: { wanted: 0, found: 0, lookedUp: 0, failed: 0 },
				};
			}
			const library = yield* listLibrary;
			const replace = yield* readReplace;
			const pinned = yield* pins.get;
			const cached = yield* verdicts.get;
			const now = Date.now();
			const want = library.filter((e) => wanted(e, def.offers, replace));
			// Verdicts of listed entries only, so the cache follows the library.
			const next: Record<string, Verdict> = {};
			for (const e of library) {
				const v = cached[e.id];
				if (v) next[e.id] = v;
			}
			const todo = want.filter(
				(e) =>
					!isFresh(cached[e.id], identityOf(e, pinned[e.id]), matcher, now),
			);
			let stopped: string | undefined;
			let retryAfterMs: number | undefined;
			let failed = 0;
			let lastError = "";
			yield* Effect.forEach(
				todo,
				(e) =>
					Effect.suspend(() =>
						stopped !== undefined
							? Effect.void
							: resolve(e, cfg, pinned[e.id], now).pipe(
									Effect.tap((v) =>
										Effect.sync(() => {
											next[e.id] = v;
										}),
									),
									Effect.catchCause((cause) =>
										Effect.sync(() => {
											const err = Cause.squash(cause);
											if (err instanceof SourceRateLimited) {
												stopped ??= "The source asked to slow down.";
												retryAfterMs = err.retryAfterMs ?? 60_000;
											} else if (err instanceof SourceUnauthorized) {
												stopped ??= err.reason;
											} else {
												failed += 1;
												lastError = describe(err);
											}
										}),
									),
								),
					),
				{ concurrency, discard: true },
			);
			yield* verdicts.update(() => next);
			const entries = want
				.flatMap((e) => {
					const found = next[e.id]?.found;
					const row = found ? toRow(e.id, found, def.offers) : undefined;
					return row ? [row] : [];
				})
				.sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
			if (retryAfterMs !== undefined) {
				yield* Effect.forkDetach(
					Effect.sleep(Duration.millis(retryAfterMs)).pipe(
						Effect.andThen(Queue.offer(kick, undefined)),
					),
				);
			}
			if (failed > 0) {
				yield* Effect.logWarning(
					`${failed} of ${todo.length} lookups failed: ${lastError}`,
				);
			}
			const allFailed = todo.length > 0 && failed === todo.length;
			const reason =
				stopped ??
				(allFailed ? `Lookups aren't working — ${lastError}` : undefined);
			yield* Ref.set(status, {
				ready: reason === undefined,
				...(reason !== undefined ? { reason } : {}),
				wanted: want.length,
				found: entries.length,
				lastRun: Date.now(),
				searchable,
			});
			return {
				entries: entries as ReadonlyArray<MetadataEntry>,
				report: {
					wanted: want.length,
					found: entries.length,
					lookedUp: todo.length,
					failed,
				},
			};
		});

		const engine = yield* makeSyncEngine<
			RoundReport,
			ReadonlyArray<MetadataEntry>,
			Env
		>({
			compute: () => round,
			apply: (entries) =>
				host
					.request("PUT", `/library/metadata/${def.name}`, {
						matching: def.matching,
						entries,
					})
					.pipe(Effect.asVoid),
			// In memory: the first round after a restart always pushes, later ones only on change.
			lastSync: {
				get: Ref.get(lastSync),
				set: (last) => Ref.set(lastSync, last),
			},
			settings: Effect.succeed({
				pollInterval: poll,
				watch: false,
				debounce: Duration.seconds(5),
				watchDirs: [],
			}),
		});
		const resync = (reason: "manual" | "library-change") =>
			engine.sync(reason).pipe(Effect.ignore);

		/** The entry, the config, and the verdict or a live match for it. */
		const context = (entryId: string) =>
			Effect.gen(function* () {
				const entry = (yield* listLibrary).find((e) => e.id === entryId);
				if (!entry) return undefined;
				const cfg = yield* cfgService.load;
				const pin = (yield* pins.get)[entryId];
				const verdict = (yield* verdicts.get)[entryId];
				const match =
					verdict?.match ??
					(yield* def
						.match(ownView(entry), cfg, pin)
						.pipe(Effect.orElseSucceed(() => null)));
				return { entry, cfg, pin, verdict, match };
			});

		const handler = async (req: Request): Promise<Response> => {
			const url = new URL(req.url);
			const route = url.pathname.slice("/__metadata/".length);
			if (route === "status" && req.method === "GET") {
				return json(await run(Ref.get(status)));
			}
			const body =
				req.method === "GET"
					? undefined
					: ((await req.json().catch(() => undefined)) as
							| Record<string, unknown>
							| undefined);
			const entryId = url.searchParams.get("entry") ?? body?.entry;
			if (typeof entryId !== "string" || !validEntryId(entryId)) {
				return json({ error: "not a library entry id" }, 400);
			}
			try {
				const found = await run(context(entryId));
				if (!found) return json({ error: "no such entry" }, 404);
				const { entry, cfg, pin, verdict, match } = found;
				if (route === "match" && req.method === "GET") {
					return json({ match, pinned: pin !== undefined });
				}
				if (route === "match" && req.method === "PUT") {
					const key = body?.key;
					if (key !== null && (typeof key !== "string" || key.length > 256)) {
						return json({ error: "key must be a string or null" }, 400);
					}
					await run(
						pins.update((p) => {
							const rest = { ...p };
							delete rest[entryId];
							return key === null ? rest : { ...rest, [entryId]: key };
						}),
					);
					const v = await run(
						resolve(entry, cfg, key ?? undefined, Date.now()),
					);
					await run(verdicts.update((all) => ({ ...all, [entryId]: v })));
					await run(resync("manual"));
					return json({ match: v.match, pinned: key !== null });
				}
				if (route === "search" && req.method === "POST") {
					const term = body?.term;
					if (!def.search)
						return json({ error: "this source can't search" }, 404);
					if (
						typeof term !== "string" ||
						term.trim() === "" ||
						term.length > 200
					) {
						return json({ error: "term must be 1–200 characters" }, 400);
					}
					const out = await run(def.search(term.trim(), ownView(entry), cfg));
					return json({
						candidates: out.slice(0, 20).map((c) => ({
							key: c.key,
							label: c.label,
							thumb: isHttpUrl(c.thumb) ? c.thumb : null,
						})),
					});
				}
				if (route === "images" && req.method === "GET") {
					const kind = url.searchParams.get("kind") as ArtKind;
					if (!ART_KINDS.includes(kind)) {
						return json(
							{ error: "kind must be portrait, hero, logo or header" },
							400,
						);
					}
					if (!match || !(def.offers.art ?? []).includes(kind)) {
						return json({ images: [] });
					}
					const images: ReadonlyArray<Image> = def.images
						? await run(def.images(match, kind, cfg))
						: [verdict?.found?.art?.[kind]]
								.filter(isHttpUrl)
								.map((u) => ({ url: u }));
					return json({
						images: images
							.filter((i) => isHttpUrl(i.url))
							.slice(0, 60)
							.map((i) => ({
								...i,
								...(i.thumb !== undefined && !isHttpUrl(i.thumb)
									? { thumb: undefined }
									: {}),
							})),
					});
				}
				return json({ error: "no such route" }, 404);
			} catch (cause) {
				return json(
					{ error: "the source failed", issue: describe(cause) },
					502,
				);
			}
		};

		yield* serveUi({
			title: def.title ?? def.name,
			category: "metadata",
			config: { schema: def.configSchema, service: cfgService },
			metadata: handler,
		});

		// Another source's change can open or close a gap here; our own push is not news.
		yield* Effect.acquireRelease(
			Effect.sync(() =>
				host.facade.events.on("library.changed", (ev) => {
					if (ev.source !== def.name) Queue.offerUnsafe(kick, undefined);
				}),
			),
			(off) => Effect.sync(off),
		);
		yield* Effect.forkScoped(
			Stream.fromQueue(kick).pipe(
				Stream.debounce(Duration.seconds(5)),
				Stream.runForEach(() => resync("library-change")),
			),
		);
		yield* engine.start;
		yield* Effect.forkScoped(
			Stream.runForEach(cfgService.changes, () => engine.reconfigure),
		);
		yield* Effect.never;
	});

	const kitDef: PluginKitDef<UiServeError, never> = {
		name: def.name,
		...(def.version !== undefined ? { version: def.version } : {}),
		layer: Layer.empty,
		main,
	};

	const standardCommands: Record<string, CliCommand<never>> = {
		lookup: {
			summary:
				"match one library entry and print what this source has (lookup <id>)",
			run: (argv) =>
				Effect.gen(function* () {
					const id = argv[0];
					const entry = (yield* listLibrary).find((e) => e.id === id);
					if (!entry) {
						console.error(`no library entry ${id ?? "(none given)"}`);
						process.exitCode = 2;
						return;
					}
					const cfg = yield* (yield* config).load;
					const v = yield* resolve(entry, cfg, undefined, Date.now());
					console.log(
						JSON.stringify({ match: v.match, found: v.found }, null, 2),
					);
				}),
		},
		uninstall: {
			summary:
				"remove everything this source filled, and its place in the order",
			run: () =>
				Effect.gen(function* () {
					yield* (yield* HostClient).request(
						"DELETE",
						`/library/metadata/${def.name}`,
					);
					console.log(`${def.name}: removed from Art & Metadata`);
				}),
		},
	};

	return {
		def: definePluginKit(kitDef),
		cli: (argv) =>
			runPluginCli({
				def: kitDef,
				commands: { ...standardCommands, ...(def.commands ?? {}) },
				...(argv !== undefined ? { argv } : {}),
			}),
	};
};
