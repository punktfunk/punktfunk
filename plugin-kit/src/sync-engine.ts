// The generic sync engine — the poll/watch/debounce/coalesce/fingerprint machinery that
// was ~duplicated between rom-manager and playnite, as one Effect service.
//
// Semantics are a faithful port of the original Engine guard:
//   - single-flight: a sync while one runs records a pending trigger and returns
//     AlreadyRunning; the running pass re-fires once ("coalesced") when it finishes
//   - content fingerprint (sha256 of the entries JSON) skips the apply when unchanged —
//     except for the two reasons a person is waiting on the answer (`ALWAYS_APPLY`)
//   - interval poll + best-effort fs watchers (recursive where the OS supports it, top-dir
//     fallback on Linux) with debounce; the poll is the real safety net on SMB/NFS
//   - every transition publishes a SyncStatus (the UI's SSE feed)
// All loops live in a private scope that `reconfigure` closes and rebuilds, and the whole
// engine tears down with the Scope it was constructed in.
import { createHash } from "node:crypto";
import * as fs from "node:fs";
import {
	Cause,
	Deferred,
	Duration,
	Effect,
	Exit,
	PubSub,
	Queue,
	Ref,
	Scope,
	Stream,
} from "effect";
import { SyncError } from "./errors.js";

export type SyncReason =
	| "startup"
	| "poll"
	| "fs-change"
	| "config-change"
	| "manual"
	| "coalesced";

export interface LastSync {
	readonly fingerprint: string;
	readonly count: number;
	readonly at: number;
}

export type SyncOutcome<Report> =
	| {
			readonly _tag: "Applied";
			readonly report: Report;
			readonly count: number;
	  }
	| { readonly _tag: "Unchanged"; readonly report: Report }
	| { readonly _tag: "AlreadyRunning" };

export interface SyncStatus<Report> {
	readonly syncing: boolean;
	readonly lastReport?: Report;
	readonly lastSync?: LastSync;
}

export interface SyncSettings {
	readonly pollInterval: Duration.Duration;
	readonly watch: boolean;
	readonly debounce: Duration.Duration;
	readonly watchDirs: ReadonlyArray<string>;
	/**
	 * Floor between two `fs-change` syncs. The debounce collapses a BURST of events into one
	 * sync, but it extends on every event and so cannot bound how often a busy launcher makes
	 * us re-walk the library — Steam writes to its dirs the whole time a game runs, and one
	 * field log carried 102 `fs-change` syncs in 27 minutes. This caps the RATE: at most one
	 * fs-change sync per interval, and every change that lands inside it coalesces into exactly
	 * one trailing sync. Default `30 s` (see `DEFAULT_FS_CHANGE_MIN_INTERVAL`).
	 */
	readonly minInterval?: Duration.Duration;
}

/**
 * Sync reasons that push to the host even when the fingerprint says nothing changed.
 *
 * A fingerprint match means WE would compute the same entries again — NOT that the host still
 * holds them. The host may accept a payload and store less of it than was sent: an art path
 * outside its allowed roots is stripped and the games kept (deliberately — a cover must not cost
 * a library), and a launcher tile it cannot open is dropped the same way. Once that happens the
 * plugin's fingerprint is a permanent "no changes": the operator fixes the host side, nothing
 * re-publishes, and the only way out is to delete the plugin's cache file. That was real
 * field advice for a portable-Playnite library whose 70 covers were dropped.
 *
 * So the two triggers with a person behind them always apply. `startup` is the restart every
 * operator reaches for, and `manual` is the console's Sync-now button and the CLI's `sync` —
 * both mean "publish my library NOW", and answering "no changes" to that is the trap. The loop
 * reasons (`poll`, `fs-change`, `config-change`, `coalesced`) keep the short-circuit, which is
 * where it earns its keep: they are what would otherwise PUT the whole library every few minutes.
 */
const ALWAYS_APPLY: ReadonlySet<SyncReason> = new Set<SyncReason>([
	"startup",
	"manual",
]);

/** `SyncSettings.minInterval` when a plugin does not set one. */
export const DEFAULT_FS_CHANGE_MIN_INTERVAL: Duration.Duration =
	Duration.seconds(30);

/**
 * How long `startLoops` waits for the forked watch fiber to reach `openWatchers`. That acquire
 * is a synchronous `fs.watch`, so 5 s can only mean the fiber went away first — a concurrent
 * `reconfigure` closing the scope, or shutdown — and an unbounded wait there hangs startup.
 */
const WATCHER_REGISTER_TIMEOUT: Duration.Duration = Duration.seconds(5);

export interface SyncEngineOptions<
	Report,
	Entries extends ReadonlyArray<unknown>,
	R,
> {
	/** Produce the full desired state. Pure of host effects — `apply` does the write. */
	readonly compute: (
		reason: SyncReason,
	) => Effect.Effect<
		{ readonly entries: Entries; readonly report: Report },
		unknown,
		R
	>;
	/** Push the desired state to the host (usually `ProviderClient.reconcile`). */
	readonly apply: (entries: Entries) => Effect.Effect<void, unknown, R>;
	/** Override the content fingerprint (default: sha256 of the entries JSON). */
	readonly fingerprint?: (entries: Entries) => string;
	/** Durable fingerprint storage (usually the plugin's CacheStore). */
	readonly lastSync: {
		readonly get: Effect.Effect<LastSync | undefined, never, R>;
		readonly set: (last: LastSync) => Effect.Effect<void, never, R>;
	};
	/** Re-read on every `reconfigure` — loops restart with fresh settings. */
	readonly settings: Effect.Effect<SyncSettings, never, R>;
}

export interface SyncEngine<Report> {
	readonly sync: (
		reason: SyncReason,
	) => Effect.Effect<SyncOutcome<Report>, SyncError>;
	readonly status: Effect.Effect<SyncStatus<Report>>;
	/** Emits on every sync start/finish — the UI's SSE feed. */
	readonly changes: Stream.Stream<SyncStatus<Report>>;
	/** Initial sync, then start loops. Returns after filesystem watchers are registered. */
	readonly start: Effect.Effect<void>;
	/** Restart loops with fresh settings, then sync("config-change"). */
	readonly reconfigure: Effect.Effect<void>;
}

const defaultFingerprint = (entries: unknown): string =>
	createHash("sha256").update(JSON.stringify(entries)).digest("hex");

/** Best-effort watcher set over `dirs`: recursive where supported, top-dir fallback. */
const openWatchers = (
	dirs: ReadonlyArray<string>,
	onEvent: () => void,
): fs.FSWatcher[] => {
	const out: fs.FSWatcher[] = [];
	for (const dir of dirs) {
		try {
			out.push(fs.watch(dir, { recursive: true }, onEvent));
		} catch {
			try {
				out.push(fs.watch(dir, onEvent));
			} catch {
				// unwatchable (poll covers it)
			}
		}
	}
	return out;
};

export const makeSyncEngine = <
	Report,
	Entries extends ReadonlyArray<unknown>,
	R,
>(
	opts: SyncEngineOptions<Report, Entries, R>,
): Effect.Effect<SyncEngine<Report>, never, R | Scope.Scope> =>
	Effect.gen(function* () {
		const ctx = yield* Effect.context<R>();
		const run = <A, E>(eff: Effect.Effect<A, E, R>): Effect.Effect<A, E> =>
			Effect.provide(eff, ctx);
		const fingerprint = opts.fingerprint ?? defaultFingerprint;

		const flags = yield* Ref.make({ syncing: false, pending: false });
		const lastReport = yield* Ref.make<Report | undefined>(undefined);
		const hub = yield* PubSub.unbounded<SyncStatus<Report>>();

		const status: Effect.Effect<SyncStatus<Report>> = Effect.gen(function* () {
			const f = yield* Ref.get(flags);
			return {
				syncing: f.syncing,
				lastReport: yield* Ref.get(lastReport),
				lastSync: yield* run(opts.lastSync.get),
			};
		});
		const publish = status.pipe(
			Effect.flatMap((s) => PubSub.publish(hub, s)),
			Effect.asVoid,
		);

		const doSync = (
			reason: SyncReason,
		): Effect.Effect<SyncOutcome<Report>, SyncError> =>
			Effect.gen(function* () {
				const { entries, report } = yield* run(opts.compute(reason)).pipe(
					Effect.mapError((cause) => new SyncError({ reason, cause })),
				);
				yield* Ref.set(lastReport, report);
				const fp = fingerprint(entries);
				const prev = yield* run(opts.lastSync.get);
				if (!ALWAYS_APPLY.has(reason) && prev?.fingerprint === fp) {
					yield* Effect.log(
						`sync (${reason}): no changes (${entries.length} entries)`,
					);
					return { _tag: "Unchanged", report } as const;
				}
				yield* run(opts.apply(entries)).pipe(
					Effect.mapError((cause) => new SyncError({ reason, cause })),
				);
				yield* run(
					opts.lastSync.set({
						fingerprint: fp,
						count: entries.length,
						at: Date.now(),
					}),
				);
				yield* Effect.log(
					`sync (${reason}): reconciled ${entries.length} entries`,
				);
				return { _tag: "Applied", report, count: entries.length } as const;
			});

		// Nothing may kill a loop: a scan that throws is a defect, not a typed failure, and
		// `Effect.catch` alone would let it end the fiber without a word.
		const safeSync = (reason: SyncReason): Effect.Effect<void> =>
			sync(reason).pipe(
				Effect.catchCause((cause) =>
					Effect.logWarning(
						`sync (${reason}) failed: ${Cause.pretty(cause).split("\n").slice(0, 8).join("\n")}`,
					),
				),
				Effect.asVoid,
			);

		const sync = (
			reason: SyncReason,
		): Effect.Effect<SyncOutcome<Report>, SyncError> =>
			Ref.modify(flags, (f) =>
				f.syncing
					? ([false, { ...f, pending: true }] as const)
					: ([true, { ...f, syncing: true }] as const),
			).pipe(
				Effect.flatMap((acquired) => {
					if (!acquired) {
						return Effect.succeed({ _tag: "AlreadyRunning" } as const);
					}
					return publish.pipe(
						Effect.andThen(doSync(reason)),
						Effect.ensuring(
							Effect.gen(function* () {
								const pending = yield* Ref.modify(
									flags,
									(f) =>
										[f.pending, { syncing: false, pending: false }] as const,
								);
								yield* publish;
								if (pending) {
									yield* Effect.forkDetach(safeSync("coalesced"));
								}
							}),
						),
					);
				}),
			);

		// ---------------------------------------------------------------- loops
		const loopScope = yield* Ref.make<Scope.Closeable | undefined>(undefined);

		const stopLoops = Effect.gen(function* () {
			const prev = yield* Ref.getAndSet(loopScope, undefined);
			if (prev) yield* Scope.close(prev, Exit.void);
		});

		const startLoops: Effect.Effect<void> = Effect.gen(function* () {
			yield* stopLoops;
			const scope = yield* Scope.make();
			yield* Ref.set(loopScope, scope);
			const settings = yield* run(opts.settings);

			const pollLoop = Effect.forever(
				Effect.sleep(settings.pollInterval).pipe(
					Effect.andThen(safeSync("poll")),
				),
			);
			yield* Effect.forkIn(pollLoop, scope);

			if (settings.watch && settings.watchDirs.length > 0) {
				// The acquire below runs inside the forked feed, so `start` would otherwise return
				// before any `fs.watch` exists and lose every write until the next poll — 15 minutes
				// on a library plugin's default. Hold `start` until the watchers are live.
				const watching = yield* Deferred.make<void>();
				const watchStream = Stream.callback<void>((queue) =>
					Effect.acquireRelease(
						Effect.sync(() =>
							openWatchers(settings.watchDirs, () => {
								Queue.offerUnsafe(queue, undefined);
							}),
						).pipe(Effect.tap(() => Deferred.succeed(watching, undefined))),
						(watchers) =>
							Effect.sync(() => {
								for (const w of watchers) {
									try {
										w.close();
									} catch {
										// already closed
									}
								}
							}),
					),
				);
				// Debounce collapses a burst; the sliding queue of ONE plus the hold below caps the
				// rate (see `SyncSettings.minInterval`). Debounced events land in the queue and
				// coalesce there while a sync runs or the hold sleeps, so a launcher that never
				// stops writing costs one sync per interval — and the change it made is never
				// lost, because the queue is drained by exactly one trailing sync.
				const minInterval =
					settings.minInterval ?? DEFAULT_FS_CHANGE_MIN_INTERVAL;
				const kick = yield* Queue.sliding<void>(1);
				const feed = watchStream.pipe(
					Stream.debounce(settings.debounce),
					Stream.runForEach(() => Queue.offer(kick, undefined)),
				);
				yield* Effect.forkIn(feed, scope);
				yield* Deferred.await(watching).pipe(
					Effect.timeoutOrElse({
						duration: WATCHER_REGISTER_TIMEOUT,
						orElse: () =>
							Effect.die(new Error("register the filesystem watchers")),
					}),
				);
				const drain = Effect.forever(
					Queue.take(kick).pipe(
						Effect.andThen(safeSync("fs-change")),
						Effect.andThen(Effect.sleep(minInterval)),
					),
				);
				yield* Effect.forkIn(drain, scope);
			}
		});

		// Loop teardown rides the construction Scope (plugin shutdown).
		yield* Effect.addFinalizer(() => stopLoops);

		return {
			sync,
			status,
			changes: Stream.fromPubSub(hub),
			start: safeSync("startup").pipe(Effect.andThen(startLoops)),
			reconfigure: startLoops.pipe(Effect.andThen(safeSync("config-change"))),
		} satisfies SyncEngine<Report>;
	});
