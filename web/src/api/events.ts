// The host's event stream, wired to React Query's cache.
//
// The host publishes every lifecycle transition on `GET /api/v1/events` as SSE — client
// connect/disconnect, session and stream start/end, pairing decisions, display create/release,
// library/store/plugin changes, update availability, host start/stop. Nothing consumed it: the
// console learned about all of it by asking again on ten separate timers, so a change was up to
// 5 s stale, two pages could disagree with each other while you looked at them, and the Library
// page — which polls not at all — never noticed a newly installed game until a full reload.
//
// This subscribes once for the whole app and invalidates exactly the queries an event affects.
// It does NOT carry data into the cache: the REST snapshots stay the source of truth, and an event
// only says "this is stale now". That keeps the wire format additive-only (a kind we don't know
// costs us nothing) and means a missed event degrades to the polling behaviour we already had.
//
// Transport notes:
//   - Same-origin, so the sealed session cookie rides along and the BFF injects the mgmt bearer;
//     no auth work here. `EventSource` reconnects on its own and replays with `Last-Event-ID`,
//     which h3's proxy forwards, so a dropped connection resumes from the host's ring.
//   - The host sends a keep-alive comment every 15 s; the Bun entry's idle timeout is set above
//     that (nitro-entry/bun-https.mjs) so we don't sever our own stream.
//   - An `event: dropped` frame means we fell off the ring and must resync — invalidate everything.
//   - A connection with no cursor replays the host's whole ring (that is what fills the activity
//     feed on a page load), then `event: live`. Only a knock after that marker toasts: a replayed
//     one is history the pending list already shows, or a device paired an hour ago.
import { type QueryClient, useQueryClient } from "@tanstack/react-query";
import { toast } from "@unom/ui/toast";
import { useEffect, useSyncExternalStore } from "react";
import { getListPairedClientsQueryKey } from "@/api/gen/clients/clients";
import { getGetDiagnosticsQueryKey } from "@/api/gen/diagnostics/diagnostics";
import { getGetDisplayStateQueryKey } from "@/api/gen/display/display";
import {
	getGetHostSettingsQueryKey,
	getGetStatusQueryKey,
} from "@/api/gen/host/host";
import {
	getGetLibraryQueryKey,
	getListLibraryScannersQueryKey,
} from "@/api/gen/library/library";
import {
	getListNativeClientsQueryKey,
	getListPendingDevicesQueryKey,
} from "@/api/gen/native/native";
import { getGetPairingStatusQueryKey } from "@/api/gen/pairing/pairing";
import { getGetPluginAccessQueryKey } from "@/api/gen/plugin-access/plugin-access";
import { getGetRecentSessionsQueryKey } from "@/api/gen/session/session";
import { getGetUpdateStatusQueryKey } from "@/api/gen/update/update";
import { boostPluginPolling, PLUGINS_KEY } from "@/api/plugins";
import { storeKeys } from "@/api/store";
import { m } from "@/paraglide/messages";
import { type ActivityEntry, mergeActivity } from "./activity-ring";

export type { ActivityEntry } from "./activity-ring";

/** Snapshots invalidated by one event kind. `plugins.changed` also refreshes folder access;
 * unknown kinds stay ignored, and generated helpers supply React Query's readonly keys. */
function keysFor(kind: string): readonly (readonly unknown[])[] {
	const status = [getGetStatusQueryKey()];
	switch (kind) {
		// Anything that changes what the host is doing right now moves the dashboard's status.
		case "client.connected":
		case "client.disconnected":
		case "session.started":
		case "stream.started":
		case "stream.stopped":
		case "game.running":
		case "game.exited":
			return status;
		// Plus the summary card: a session that just ended is the one it exists to show.
		case "session.ended":
			return [...status, getGetRecentSessionsQueryKey()];
		// A display appearing or going away changes the live list, and its policy card shows
		// "in effect" values derived from the same state.
		case "display.created":
		case "display.released":
			return [...status, getGetDisplayStateQueryKey()];
		// A knock (and a denial clearing one) changes the pending list itself — which is the one
		// an operator sits and waits on. It used to refresh only on its own 10 s timer.
		case "pairing.pending":
		case "pairing.denied":
			return [
				...status,
				getGetPairingStatusQueryKey(),
				getListPendingDevicesQueryKey(),
			];
		// A completed pairing also adds a device to whichever plane's list is on screen.
		case "pairing.completed":
			return [
				...status,
				getGetPairingStatusQueryKey(),
				getListPairedClientsQueryKey(),
				getListNativeClientsQueryKey(),
			];
		// The base key with no params is a PREFIX of every parameterised library query, and React
		// Query invalidates by prefix — so this catches the Dashboard's and the Library page's alike.
		// The source list counts entries per provider, so it moves with the library.
		case "library.changed":
			return [getGetLibraryQueryKey(), getListLibraryScannersQueryKey()];
		case "update.available":
		case "update.applied":
			return [getGetUpdateStatusQueryKey()];
		// Registration and folder-access changes move plugin views; store changes move packages.
		// A new pending request can add a source line, so the source list moves too.
		case "plugins.changed":
			return [
				PLUGINS_KEY,
				storeKeys.catalog,
				storeKeys.installed,
				storeKeys.runtime,
				getGetPluginAccessQueryKey(),
				getListLibraryScannersQueryKey(),
			];
		case "store.changed":
			return [
				PLUGINS_KEY,
				storeKeys.catalog,
				storeKeys.installed,
				storeKeys.runtime,
			];
		// A restart-class change also moves the restart-pending check on Home.
		case "settings.changed":
			return [getGetHostSettingsQueryKey(), getGetDiagnosticsQueryKey()];
		// The host came back: everything we hold predates it.
		case "host.started":
			return [];
		default:
			return [];
	}
}

/**
 * Mark one key's data wrong and refetch it.
 *
 * `refetchType: "all"` rather than the default `"active"`: an event says the HOST changed, so every
 * cached copy is wrong, whether or not a mounted component happens to be observing it right now.
 * The default only refetches queries with a live observer, which silently did nothing for a page
 * that had just been re-rendered — the cache stayed marked-stale-but-unfetched and the screen kept
 * showing the old answer.
 */
function invalidate(qc: QueryClient, queryKey: readonly unknown[]): void {
	qc.invalidateQueries({ queryKey, refetchType: "all" });
}

/** Invalidate every query — used on `dropped` (we fell off the ring) and on `host.started`. */
function resyncAll(qc: QueryClient): void {
	qc.invalidateQueries({ refetchType: "all" });
}

// ---------------------------------------------------------------------------------------------
// The activity log.
//
// The same frames that drive invalidation are also, in themselves, the answer to "what has this
// host been doing?" — a question the console could not answer at all. Nothing else records this:
// the REST snapshots describe the present, and the host's own log is a developer artifact, not a
// narrative. So keep a small in-memory ring alongside the cache work.
//
// Deliberately NOT persisted and deliberately bounded: it is a live tail for someone watching, not
// an audit trail, and a page load starts fresh from whatever the ring replays.
// ---------------------------------------------------------------------------------------------

let activity: ActivityEntry[] = [];
const activityListeners = new Set<() => void>();

/**
 * True once this page load's replay has been applied, or its deadline passed. Before that the
 * feed is not empty, it is unknown — so a card rendered from it waits instead of mounting empty
 * and filling a moment later. Mounted together with its rows, the card animates the same way on
 * every path (reload, navigation, a slow status query); mounted early, it depended on timing.
 */
let activityReady = false;

/** Fold frames into the ring and notify once — or not at all when none of them was new. */
function commitActivity(batch: ActivityEntry[]): void {
	const next = mergeActivity(activity, batch);
	if (next === activity) return;
	activity = next;
	for (const l of activityListeners) l();
}

/** The activity tail, newest first. Re-renders as frames arrive. */
export function useActivity(): ActivityEntry[] {
	return useSyncExternalStore(
		(cb) => {
			activityListeners.add(cb);
			return () => activityListeners.delete(cb);
		},
		() => activity,
		// The server has no stream, so SSR renders an empty feed and hydrates into the live one.
		() => EMPTY_ACTIVITY,
	);
}

const EMPTY_ACTIVITY: ActivityEntry[] = [];

/** Whether the feed holds what it is going to hold yet — see `activityReady`. */
export function useActivityReady(): boolean {
	return useSyncExternalStore(
		(cb) => {
			activityListeners.add(cb);
			return () => activityListeners.delete(cb);
		},
		() => activityReady,
		() => false,
	);
}

/** Every kind we act on. A kind the host adds later simply has no listener — never a mis-handle. */
const KINDS = [
	"client.connected",
	"client.disconnected",
	"session.started",
	"session.ended",
	"stream.started",
	"stream.stopped",
	"game.running",
	"game.exited",
	"pairing.pending",
	"pairing.completed",
	"pairing.denied",
	"display.created",
	"display.released",
	"library.changed",
	"update.available",
	"update.applied",
	"plugins.changed",
	"store.changed",
	"settings.changed",
	"host.started",
] as const;

// ---------------------------------------------------------------------------------------------
// The connection is a module-level singleton, refcounted, NOT a per-component resource.
//
// It has to be. The subscription is app-lifetime, but the component that asks for it is not:
// during hydration TanStack Start mounts the app shell and discards it again ~15 ms later
// (measured), which ran an effect cleanup with no matching re-mount. Tied to that effect, the
// stream opened, closed, and never came back — the console looked subscribed and received nothing.
//
// So: `open()` hands out a reference and only the LAST release closes the socket, after a short
// grace period, so a remount inside that window re-attaches to the live stream instead of
// reconnecting. `EventSource` handles reconnection itself and replays with `Last-Event-ID`, which
// the SSE route forwards.
// ---------------------------------------------------------------------------------------------
let source: EventSource | null = null;
let refs = 0;
let closeTimer: ReturnType<typeof setTimeout> | null = null;
/** The client to invalidate against — one per page load, re-pointed if React hands us a new one. */
let client: QueryClient | null = null;

/** How long the stream survives with no subscribers, so a hydration blip doesn't reconnect. */
const CLOSE_GRACE_MS = 10_000;

/** False until the host's `live` marker: frames before it are ring replay, not news. */
let live = false;

// A connection replays the host's whole ring before `live`: 161 frames in 43 ms on a real host, 114
// of them `library.changed`. Handled one at a time, all of it ran in a single 600 ms task — every
// frame rendered the feed and evicted a row whose exit animation never got a frame, so the card
// grew to 157 rows, and every frame invalidated its queries, so /library was fetched 109 times on
// one reload. The replay is held instead and applied ONCE: one render, each query invalidated once.
let replayed: ActivityEntry[] = [];
const replayedKeys = new Map<string, readonly unknown[]>();
let replayedResync = false;
let replayedBoost = false;
let replayTimer: ReturnType<typeof setTimeout> | null = null;

/** An older host sends no `live` marker: past this, apply what arrived rather than hold it. */
const REPLAY_DEADLINE_MS = 3_000;

function startReplay(): void {
	live = false;
	replayed = [];
	replayedKeys.clear();
	replayedResync = false;
	replayedBoost = false;
	if (replayTimer) clearTimeout(replayTimer);
	replayTimer = setTimeout(finishReplay, REPLAY_DEADLINE_MS);
}

/** Note what a replayed frame WOULD have done, deduplicated, instead of doing it now. */
function holdReplayed(kind: string, entry: ActivityEntry | null): void {
	if (entry) replayed.push(entry);
	for (const key of keysFor(kind)) replayedKeys.set(JSON.stringify(key), key);
	if (kind === "plugins.changed" || kind === "store.changed")
		replayedBoost = true;
	if (kind === "host.started") replayedResync = true;
}

/** Apply the held replay in one pass and go live. Safe to call twice: the second has nothing. */
function finishReplay(): void {
	if (replayTimer) {
		clearTimeout(replayTimer);
		replayTimer = null;
	}
	live = true;
	commitActivity(replayed);
	replayed = [];
	if (!activityReady) {
		activityReady = true;
		for (const l of activityListeners) l();
	}
	if (client) {
		// A restart in the replay makes every snapshot stale; the individual keys are a subset.
		if (replayedResync) resyncAll(client);
		else for (const key of replayedKeys.values()) invalidate(client, key);
		if (replayedBoost) boostPluginPolling();
	}
	replayedKeys.clear();
	replayedResync = false;
	replayedBoost = false;
}

function attach(): void {
	if (source) return;
	source = new EventSource("/api/v1/events");
	// A stream that never opens still settles the feed, so the card is not held back forever.
	if (!replayTimer) replayTimer = setTimeout(finishReplay, REPLAY_DEADLINE_MS);
	// Every (re)connect replays first; `open` fires before any frame, on auto-reconnect too.
	source.addEventListener("open", startReplay);
	source.addEventListener("live", finishReplay);
	for (const kind of KINDS) {
		source.addEventListener(kind, (ev) => {
			const entry = parseEntry(kind, ev);
			if (!live) {
				holdReplayed(kind, entry);
				return;
			}
			// Record it first: the feed should show an event even for a kind we invalidate nothing for.
			if (entry) commitActivity([entry]);
			// A knock needs someone to act on it, and it can land on any page — so it is the one
			// event that interrupts rather than waiting to be noticed on the Pairing page. Replayed
			// knocks never reach here: they are history the pending list already shows.
			if (kind === "pairing.pending") announceKnock(entry);
			if (!client) return;
			// The installed set changed — but the runner is probably still restarting, so keep
			// checking for a while rather than trusting this one refetch (see boostPluginPolling).
			if (kind === "plugins.changed" || kind === "store.changed")
				boostPluginPolling();
			for (const key of keysFor(kind)) invalidate(client, key);
			// `host.started` names no keys — the host is NEW, so everything we hold predates it.
			if (kind === "host.started") resyncAll(client);
		});
	}
	// We fell off the host's ring — every snapshot we hold may be wrong.
	source.addEventListener("dropped", () => {
		if (client) resyncAll(client);
	});
}

/**
 * Parse one SSE frame into an activity entry, or null for a malformed one — dropped, never thrown.
 * Parsing and recording are separate because a replayed frame is held, not recorded, until `live`.
 */
function parseEntry(kind: string, ev: Event): ActivityEntry | null {
	const raw = (ev as MessageEvent<string>).data;
	if (typeof raw !== "string") return null;
	try {
		const data = JSON.parse(raw) as Record<string, unknown>;
		const seq = typeof data.seq === "number" ? data.seq : Number.NaN;
		const ts = typeof data.ts_ms === "number" ? data.ts_ms : Number.NaN;
		if (!Number.isFinite(seq) || !Number.isFinite(ts)) return null;
		return { seq, ts_ms: ts, kind, data };
	} catch {
		// A frame we cannot parse is not worth breaking the stream over.
		return null;
	}
}

/**
 * Tell the operator a device is knocking, wherever they are in the console.
 *
 * The pending list is on one page, and until now nothing said a request had arrived unless you
 * were already looking at it — worst on a phone, where that page is several taps away.
 *
 * The action is a plain navigation rather than a router push: this module is not a component and
 * has no router to reach. It costs a page load, on a button someone pressed on purpose.
 */
function announceKnock(entry: ActivityEntry | null): void {
	const device = entry?.data.device as { name?: unknown } | undefined;
	const name = typeof device?.name === "string" ? device.name : null;
	toast.info(m.pairing_knock_title(), {
		description: name ? m.pairing_knock_desc({ name }) : undefined,
		action: {
			label: m.pairing_knock_action(),
			onClick: () => window.location.assign("/pairing"),
		},
	});
}

function release(): void {
	refs -= 1;
	if (refs > 0) return;
	if (closeTimer) clearTimeout(closeTimer);
	closeTimer = setTimeout(() => {
		closeTimer = null;
		if (refs > 0) return; // someone re-subscribed inside the grace window
		source?.close();
		source = null;
	}, CLOSE_GRACE_MS);
}

/**
 * Subscribe to the host's event stream. Safe to call from more than one component and safe on the
 * server (`EventSource` is browser-only, so this is a no-op during SSR).
 */
export function useHostEvents(): void {
	const qc = useQueryClient();
	useEffect(() => {
		if (typeof window === "undefined" || typeof EventSource === "undefined")
			return;
		client = qc;
		refs += 1;
		if (closeTimer) {
			clearTimeout(closeTimer);
			closeTimer = null;
		}
		attach();
		return release;
	}, [qc]);
}
