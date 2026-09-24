// SyncEngine semantics: fingerprint skip, single-flight coalescing, status feed.
import { describe, expect, test } from "bun:test";
import { Duration, Effect, Fiber, Ref, type Scope, Stream } from "effect";
import {
	type LastSync,
	makeSyncEngine,
	type SyncOutcome,
} from "../src/index.js";

interface Report {
	readonly included: number;
}

const harness = (opts?: {
	computeDelayMs?: number;
	entries?: () => ReadonlyArray<string>;
}) =>
	Effect.gen(function* () {
		const applied = yield* Ref.make(0);
		const last = yield* Ref.make<LastSync | undefined>(undefined);
		const entries = opts?.entries ?? (() => ["a", "b"]);
		const engine = yield* makeSyncEngine<Report, ReadonlyArray<string>, never>({
			compute: () =>
				Effect.suspend(() => {
					const e = entries();
					return Effect.succeed({
						entries: e,
						report: { included: e.length },
					});
				}).pipe(
					opts?.computeDelayMs
						? Effect.delay(Duration.millis(opts.computeDelayMs))
						: (x) => x,
				),
			apply: () => Ref.update(applied, (n) => n + 1),
			lastSync: {
				get: Ref.get(last),
				set: (l) => Ref.set(last, l),
			},
			settings: Effect.succeed({
				pollInterval: Duration.minutes(60),
				watch: false,
				debounce: Duration.millis(10),
				watchDirs: [],
			}),
		});
		return { engine, applied, last };
	});

const run = <A>(eff: Effect.Effect<A, unknown, Scope.Scope>): Promise<A> =>
	Effect.runPromise(Effect.scoped(eff) as Effect.Effect<A>);

describe("SyncEngine", () => {
	test("first sync applies; unchanged content skips the apply", async () => {
		// A LOOP reason, deliberately: the fingerprint skip is what keeps a 5-minute poll from
		// PUTting the whole library forever. `startup`/`manual` opt out of it (below).
		const { first, second, count } = await run(
			Effect.gen(function* () {
				const h = yield* harness();
				const first = yield* h.engine.sync("poll");
				const second = yield* h.engine.sync("poll");
				return {
					first,
					second,
					count: yield* Ref.get(h.applied),
				};
			}),
		);
		expect(first._tag).toBe("Applied");
		if (first._tag === "Applied") expect(first.count).toBe(2);
		expect(second._tag).toBe("Unchanged");
		expect(count).toBe(1);
	});

	/**
	 * The host can store LESS than it was sent (an out-of-root cover is stripped, the games kept),
	 * and the fingerprint cannot see that — it only says we would compute the same thing again. So
	 * the two triggers a person is waiting on re-publish regardless, and the fix for a mangled
	 * host-side copy is a restart or the Sync button rather than deleting the plugin's cache.
	 */
	test("startup and manual re-apply even when nothing changed", async () => {
		const counts = await run(
			Effect.gen(function* () {
				const h = yield* harness();
				yield* h.engine.sync("poll"); // first apply, fingerprint stored
				const afterPoll = yield* Ref.get(h.applied);
				const startup = yield* h.engine.sync("startup");
				const manual = yield* h.engine.sync("manual");
				// …and the loops still skip, with the same fingerprint in place.
				const loop = yield* h.engine.sync("fs-change");
				return {
					afterPoll,
					tags: [startup._tag, manual._tag, loop._tag],
					total: yield* Ref.get(h.applied),
				};
			}),
		);
		expect(counts.afterPoll).toBe(1);
		expect(counts.tags).toEqual(["Applied", "Applied", "Unchanged"]);
		expect(counts.total).toBe(3);
	});

	test("changed content re-applies", async () => {
		let call = 0;
		const { outcomes, count } = await run(
			Effect.gen(function* () {
				const h = yield* harness({
					entries: () => (call++ === 0 ? ["a"] : ["a", "b"]),
				});
				const o1 = yield* h.engine.sync("manual");
				const o2 = yield* h.engine.sync("manual");
				return { outcomes: [o1, o2], count: yield* Ref.get(h.applied) };
			}),
		);
		expect(outcomes.map((o: SyncOutcome<Report>) => o._tag)).toEqual([
			"Applied",
			"Applied",
		]);
		expect(count).toBe(2);
	});

	test("concurrent trigger returns AlreadyRunning and coalesces into a follow-up", async () => {
		const { during, count } = await run(
			Effect.gen(function* () {
				const h = yield* harness({ computeDelayMs: 50 });
				const fiber = yield* Effect.forkChild(h.engine.sync("manual"));
				yield* Effect.sleep("10 millis");
				const during = yield* h.engine.sync("manual");
				yield* Fiber.join(fiber);
				// the coalesced re-run is forked detached — give it a beat
				yield* Effect.sleep("120 millis");
				return { during, count: yield* Ref.get(h.applied) };
			}),
		);
		expect(during._tag).toBe("AlreadyRunning");
		// first sync applied; the coalesced pass found identical content → skipped
		expect(count).toBe(1);
	});

	test("status feed publishes syncing transitions", async () => {
		const statuses = await run(
			Effect.gen(function* () {
				const h = yield* harness();
				const fiber = yield* Effect.forkChild(
					h.engine.changes.pipe(Stream.take(2), Stream.runCollect),
				);
				yield* Effect.sleep("20 millis");
				yield* h.engine.sync("manual");
				return yield* Fiber.join(fiber);
			}),
		);
		expect(statuses[0]?.syncing).toBe(true);
		expect(statuses[1]?.syncing).toBe(false);
		expect(statuses[1]?.lastSync?.count).toBe(2);
	});
});

// The fs-change RATE cap (`SyncSettings.minInterval`). A debounce collapses a burst but extends
// on every event, so a launcher that keeps writing (Steam, while a game runs) drove one field log
// to 102 fs-change syncs in 27 minutes. With the cap, sustained churn costs one sync per interval
// and still lands one trailing sync for whatever changed inside it.
describe("SyncEngine loop survives a throwing scan", () => {
	test("a scan that throws once does not end the poll loop", async () => {
		const computes = await run(
			Effect.gen(function* () {
				const n = yield* Ref.make(0);
				const engine = yield* makeSyncEngine<
					Report,
					ReadonlyArray<string>,
					never
				>({
					compute: () =>
						Ref.updateAndGet(n, (x) => x + 1).pipe(
							Effect.map((count) => {
								// A plugin scan has no error channel: a throw is a defect.
								if (count === 2) throw new Error("scan blew up");
								return { entries: ["a"], report: { included: 1 } };
							}),
						),
					apply: () => Effect.void,
					lastSync: { get: Effect.succeed(undefined), set: () => Effect.void },
					settings: Effect.succeed({
						pollInterval: Duration.millis(20),
						watch: false,
						debounce: Duration.millis(10),
						watchDirs: [],
					}),
				});
				yield* engine.start;
				yield* Effect.sleep(Duration.millis(300));
				return yield* Ref.get(n);
			}),
		);
		expect(computes).toBeGreaterThan(4);
	});
});

describe("SyncEngine fs-change min-interval", () => {
	test("sustained churn is capped to one fs-change sync per interval, plus one trailing", async () => {
		const fs = await import("node:fs");
		const os = await import("node:os");
		const path = await import("node:path");
		const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pf-sync-cap-"));
		try {
			const computes = await run(
				Effect.gen(function* () {
					const computed = yield* Ref.make(0);
					const last = yield* Ref.make<LastSync | undefined>(undefined);
					const engine = yield* makeSyncEngine<
						Report,
						ReadonlyArray<string>,
						never
					>({
						compute: () =>
							Ref.updateAndGet(computed, (n) => n + 1).pipe(
								// Distinct content every time, so nothing is fingerprint-skipped
								// and every sync is a real re-walk — the cost being capped.
								Effect.map((n) => ({
									entries: [`e${n}`],
									report: { included: 1 },
								})),
							),
						apply: () => Effect.void,
						lastSync: { get: Ref.get(last), set: (l) => Ref.set(last, l) },
						settings: Effect.succeed({
							pollInterval: Duration.minutes(60),
							watch: true,
							debounce: Duration.millis(20),
							minInterval: Duration.millis(400),
							watchDirs: [dir],
						}),
					});
					yield* engine.start; // startup sync (1) + the watch loops
					// Churn: a write every 25 ms for 700 ms — each one clears the 20 ms debounce,
					// so without the cap this is ~28 syncs.
					for (let i = 0; i < 28; i++) {
						fs.writeFileSync(path.join(dir, `f${i % 3}.txt`), String(i));
						yield* Effect.sleep("25 millis");
					}
					// Past the last hold, so the trailing sync has happened.
					yield* Effect.sleep("600 millis");
					return yield* Ref.get(computed);
				}),
			);
			// startup + first fs-change + one per 400 ms hold (+ trailing): well under the ~29 an
			// uncapped engine would run, and more than the startup alone (the watch works).
			expect(computes).toBeGreaterThanOrEqual(2);
			expect(computes).toBeLessThanOrEqual(6);
		} finally {
			fs.rmSync(dir, { recursive: true, force: true });
		}
	});
});

// `start` must not return before the watchers exist. The acquire that opens them runs inside a
// forked fiber, so a write landing in that window is invisible until the next poll — 15 minutes on
// a library plugin's default, and the game a person just installed looks lost.
describe("SyncEngine watch readiness", () => {
	test("a write immediately after start syncs without waiting for the poll", async () => {
		const fs = await import("node:fs");
		const os = await import("node:os");
		const path = await import("node:path");
		const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pf-sync-ready-"));
		try {
			const computes = await run(
				Effect.gen(function* () {
					const computed = yield* Ref.make(0);
					const last = yield* Ref.make<LastSync | undefined>(undefined);
					const engine = yield* makeSyncEngine<
						Report,
						ReadonlyArray<string>,
						never
					>({
						compute: () =>
							Ref.updateAndGet(computed, (n) => n + 1).pipe(
								Effect.map((n) => ({
									entries: [`e${n}`],
									report: { included: 1 },
								})),
							),
						apply: () => Effect.void,
						lastSync: { get: Ref.get(last), set: (l) => Ref.set(last, l) },
						settings: Effect.succeed({
							// An hour, so only the watcher can produce the second sync.
							pollInterval: Duration.minutes(60),
							watch: true,
							debounce: Duration.millis(20),
							minInterval: Duration.millis(400),
							watchDirs: [dir],
						}),
					});
					yield* engine.start; // startup sync (1)
					fs.writeFileSync(path.join(dir, "a.txt"), "1");
					yield* Effect.sleep("300 millis"); // past the 20 ms debounce
					return yield* Ref.get(computed);
				}),
			);
			expect(computes).toBe(2);
		} finally {
			fs.rmSync(dir, { recursive: true, force: true });
		}
	});
});
