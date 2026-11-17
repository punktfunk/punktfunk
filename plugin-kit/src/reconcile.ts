// The library-provider client, owned by the kit so plugins stop hand-copying host calls.
// The wire SCHEMAS live in ./wire.ts (browser-safe — plugin contracts re-use them); the
// transport here stays the SDK's untyped `pf.request` seam (version-skew-safe under the
// runner-bundled SDK — plugin-ui-surface.md D7).
import { Context, Effect, Layer } from "effect";
import type { HostRequestError } from "./errors.js";
import { HostClient } from "./host-client.js";
import type { ProviderEntry } from "./wire.js";

export * from "./wire.js";

/**
 * The host's liveness-report TTL, in seconds, when it does not say.
 *
 * Only a fallback for parsing an unexpected answer — the authority is the `ttlS` the host returns,
 * from `REPORT_TTL` in the host's game/runstate.rs.
 * A reporter should refresh at a fraction of this, so one missed call is not a lapse.
 */
export const DEFAULT_RUNNING_TTL_S = 90;

/** What the host echoed back for one reconciled entry — enough to tell whether a claim took. */
export interface ReconciledEntry {
	readonly id: string;
	readonly external_id?: string;
	/** The store badge the host assigned: the claim when it honoured one, else `"custom"`. */
	readonly store?: string;
}

export interface ProviderClientService {
	/**
	 * Full-replace reconcile: PUT the desired set; the host diffs by `external_id`.
	 *
	 * `store` claims that store for this provider (design D2), which is what makes the entries carry
	 * the store's own identity — deterministic `<store>:<external_id>` ids instead of opaque
	 * `custom:<id>` ones, the store's badge, and suppression of the host's matching built-in scanner
	 * so the two never double-list. One provider per store: a second claimant gets a 409.
	 *
	 * Returns the host's echoed entries so a caller can verify the claim actually took — a host
	 * predating claims ignores the query parameter silently, and the only way to notice is that the
	 * entries come back as `custom`.
	 */
	readonly reconcile: (
		providerId: string,
		entries: ReadonlyArray<ProviderEntry>,
		store?: string,
	) => Effect.Effect<ReadonlyArray<ReconciledEntry>, HostRequestError>;
	/**
	 * Report which of this provider's titles are running **right now** — the live counterpart to the
	 * static `detect` hints in {@link reconcile}.
	 *
	 * `detect` says *how to recognize* a title's process; this says *it is running*, and carries the
	 * pid where the provider knows one. For a launcher that starts games itself and is told when
	 * they stop, this is a fact the host would otherwise re-derive by scanning — and for a title
	 * with nothing to scan for (an emulated game, a manually added one, a launcher that records no
	 * install directory) could not derive at all: its lease is `untracked`, its exit is never
	 * noticed, and the streaming session outlives the game.
	 *
	 * **Send the complete set, not a delta.** Anything absent is reported stopped, so a missed
	 * event, a plugin restart or an install mid-game all self-correct on the next call.
	 *
	 * **The host expires a report** (`ttlS` in the answer) unless it is
	 * restated — which is what makes it safe for the host to keep a session open for a game it
	 * cannot see. Call this on every change **and** on a timer well inside that window while
	 * anything is running; a plugin that stops reporting simply hands tracking back to the host's
	 * process scan.
	 *
	 * Titles the host has no entry for are counted in `unknown`, not refused: a report may
	 * legitimately race its own reconcile.
	 *
	 * Fails on a host that predates the route (404) — treat that as "this host tracks games by
	 * scanning" and carry on, exactly as with any other optional capability.
	 */
	readonly reportRunning: (
		providerId: string,
		running: ReadonlyArray<RunningTitle>,
	) => Effect.Effect<RunningAccepted, HostRequestError>;
	/**
	 * Remove every entry this provider owns **and release its store claim** (the explicit-uninstall
	 * path). Releasing brings back a host's built-in scanner for the store, where one exists.
	 */
	readonly remove: (
		providerId: string,
	) => Effect.Effect<void, HostRequestError>;
}

/** One running title in a {@link ProviderClientService.reportRunning} call. */
export interface RunningTitle {
	/** The provider's own stable id — the same key its reconcile payload uses. */
	readonly external_id: string;
	/**
	 * The process the provider started for it, when it knows one. Optional, and never trusted as a
	 * bare number: the host re-resolves it and pins it to its start time before it is ever
	 * signalled, so a stale or recycled pid contributes nothing. Worth sending anyway — it is what
	 * gives "End game" something to aim at for a title the host's matcher cannot find.
	 */
	readonly pid?: number;
}

/** What the host answered to a liveness report. */
export interface RunningAccepted {
	/** How many reported titles matched an entry this provider currently publishes. */
	readonly matched: number;
	/** How many were ignored because no such entry exists (a report that raced a reconcile). */
	readonly unknown: number;
	/** Seconds the report stays authoritative without being restated. */
	readonly ttlS: number;
}

export class ProviderClient extends Context.Service<
	ProviderClient,
	ProviderClientService
>()("@punktfunk/plugin-kit/ProviderClient") {
	static readonly layer: Layer.Layer<ProviderClient, never, HostClient> =
		Layer.effect(ProviderClient)(
			Effect.gen(function* () {
				const host = yield* HostClient;
				return {
					reconcile: (providerId, entries, store) =>
						host
							.request(
								"PUT",
								`/library/provider/${providerId}${
									store ? `?store=${encodeURIComponent(store)}` : ""
								}`,
								entries,
							)
							.pipe(
								// The host answers with its resulting entries. An older host may answer
								// with something else, so treat a non-array as "no echo" rather than
								// failing the sync.
								Effect.map((body) =>
									Array.isArray(body)
										? (body as ReadonlyArray<ReconciledEntry>)
										: [],
								),
							),
					reportRunning: (providerId, running) =>
						host
							.request("PUT", `/library/provider/${providerId}/running`, {
								running,
							})
							.pipe(
								// Same posture as the reconcile echo above: the counts are a
								// diagnostic, not a contract, so a host that answers something
								// unexpected must not fail a plugin's report loop. The TTL falls
								// back to the host's own documented default.
								Effect.map((body) => {
									const b = (body ?? {}) as Record<string, unknown>;
									const num = (v: unknown, fallback: number) =>
										typeof v === "number" && Number.isFinite(v) ? v : fallback;
									return {
										matched: num(b.matched, 0),
										unknown: num(b.unknown, 0),
										ttlS: num(b.ttl_s, DEFAULT_RUNNING_TTL_S),
									} satisfies RunningAccepted;
								}),
							),
					remove: (providerId) =>
						host
							.request("DELETE", `/library/provider/${providerId}`)
							.pipe(Effect.asVoid),
				} satisfies ProviderClientService;
			}),
		);
}
