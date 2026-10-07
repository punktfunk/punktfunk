import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { inSeat } from "@/api/fetcher";
import {
	clientLogsGet,
	logsGet,
	useClientLogsList,
	useLogsGet,
} from "@/api/gen/logs/logs";
import type { ClientLogMeta } from "@/api/gen/model/clientLogMeta";
import type { LogEntry } from "@/api/gen/model/logEntry";
import type { Loadable } from "@/lib/query";
import { bundleRows, type Row, seatSource } from "./rows";

const KEEP = 5_000; // accumulated entries (client memory bound)

/**
 * The host log poll, lifted out of the viewer card.
 *
 * It lives at page level because two things now read it: the viewer, and the page's "export
 * everything" action. A second copy of this hook would mean a second cursor and a second poll
 * racing the first over the same ring, so there is exactly one and the page passes it down.
 *
 * Cursor-paged: a non-empty page advances the cursor — a new query key, so the next page fetches
 * immediately and a backlog drains fast; an empty page leaves the key unchanged and
 * `refetchInterval` paces the idle poll. Pausing (follow off) stops the interval.
 */
export const useHostLog = () => {
	const [cursor, setCursor] = useState(0);
	const [entries, setEntries] = useState<LogEntry[]>([]);
	const [follow, setFollow] = useState(true);
	const [dropped, setDropped] = useState(false);
	// Set while a poll has failed and we have not yet re-read the ring from the start.
	const [resync, setResync] = useState(false);

	const query = useLogsGet(
		{ after: cursor > 0 ? cursor : undefined },
		{
			query: {
				refetchInterval: follow ? 2_000 : false,
				// Pausing must actually pause. Stopping only the interval left React Query's default
				// focus/reconnect refetches landing, and the append effect consumed them
				// unconditionally — so tabbing away and back evicted the lines the operator had
				// paused on, from behind the pause button.
				refetchOnWindowFocus: follow,
				refetchOnReconnect: follow,
			},
		},
	);

	// Resync after the host goes away and comes back.
	//
	// The host's log ring restarts at seq 1 on every restart, while our cursor stays wherever it
	// got to. `GET /logs?after=8000` against a fresh ring is not an error — it is a permanently
	// EMPTY page (`next` echoes `after`), so the page would poll forever showing stale lines with
	// no error, no dropped badge and no way back short of a full reload. The console's own update
	// flow restarts the host, so this was reachable from two clicks away.
	//
	// A restart always breaks the poll first, so a failed query is the trigger: on the next success
	// we re-read from the start of the ring once and let the effect below decide whether the
	// sequence actually regressed.
	const failed = query.isError;
	useEffect(() => {
		if (failed) setResync(true);
	}, [failed]);
	useEffect(() => {
		if (resync && cursor !== 0) setCursor(0);
	}, [resync, cursor]);

	const data = query.data;
	useEffect(() => {
		if (!data || data.entries.length === 0) return;
		setEntries((prev) => {
			const lastSeq = prev.at(-1)?.seq ?? -1;
			// A page whose newest entry is OLDER than what we already hold can only mean the host's
			// sequence restarted underneath us — the buffer describes a host that no longer exists,
			// so replace it wholesale rather than filtering every new line away as "already seen".
			const newest = data.entries.at(-1)?.seq ?? -1;
			if (newest < lastSeq) return data.entries.slice(-KEEP);
			// Otherwise append only what's newer — dedup by the monotonic `seq`. Guards a
			// double-invoked mount effect (React StrictMode, or `data` warm in cache) from appending
			// the same page twice (duplicate rows + duplicate React keys), and makes the post-resync
			// re-read from 0 a no-op when the host did NOT restart.
			const fresh = data.entries.filter((e) => e.seq > lastSeq);
			return fresh.length ? [...prev, ...fresh].slice(-KEEP) : prev;
		});
		setDropped((d) => d || data.dropped);
		setCursor(data.next);
		setResync(false);
	}, [data]);

	const clear = useCallback(() => {
		setEntries([]);
		setDropped(false);
	}, []);

	return {
		entries,
		follow,
		setFollow,
		dropped,
		clear,
		error: query.error,
		isLoading: query.isLoading,
		refetch: query.refetch,
	};
};

/** A seat host the box can reach, and the tag its lines carry. */
export interface SeatSource {
	id: string;
	label: string;
}

/** How often a selected seat's ring is read. */
const SEAT_POLL_MS = 2_000;

/**
 * Each selected seat's own log ring, read through the box's seat proxy (`/profiles/{id}/proxy/
 * logs`) on the same cursor scheme as the host's. Only a selected seat is polled; a seat that
 * stops answering keeps the lines it gave, and one whose ring restarted is re-read from the start.
 */
export const useSeatLogs = (seats: SeatSource[], selected: Set<string>) => {
	const [rings, setRings] = useState<Record<string, LogEntry[]>>({});
	// Read by the poll, never rendered: a cursor in state would restart the poll on every page.
	const cursors = useRef<Record<string, number>>({});
	const key = seats
		.filter((s) => selected.has(seatSource(s.id)))
		.map((s) => s.id)
		.join(",");
	useEffect(() => {
		const ids = key ? key.split(",") : [];
		if (ids.length === 0) return;
		let live = true;
		const poll = async () => {
			for (const id of ids) {
				const after = cursors.current[id];
				const page = await inSeat(id, () =>
					logsGet(after ? { after } : undefined),
				).catch(() => null);
				// A restarted seat answers a stale cursor with empty pages forever, and a restart
				// always fails a poll first: after a failure, re-read the ring from its start.
				if (!page) cursors.current[id] = 0;
				if (!live || !page) continue;
				cursors.current[id] = page.next;
				setRings((prev) => {
					const held = prev[id] ?? [];
					const last = held.at(-1)?.seq ?? -1;
					const newest = page.entries.at(-1)?.seq ?? -1;
					const next =
						newest < last
							? page.entries
							: [...held, ...page.entries.filter((e) => e.seq > last)];
					return { ...prev, [id]: next.slice(-KEEP) };
				});
			}
		};
		void poll();
		const timer = setInterval(poll, SEAT_POLL_MS);
		return () => {
			live = false;
			clearInterval(timer);
		};
	}, [key]);

	return useMemo(
		() =>
			seats.flatMap((s) =>
				selected.has(seatSource(s.id))
					? (rings[s.id] ?? []).map((e) => ({
							key: `${seatSource(s.id)}:${e.seq}`,
							ts: e.ts_ms,
							level: e.level,
							target: e.target,
							msg: e.msg,
							source: seatSource(s.id),
							device: s.label,
						}))
					: [],
			),
		[seats, selected, rings],
	);
};

/** A bundle plus whatever this page has managed to fetch of it. */
export interface DeviceBundle {
	meta: ClientLogMeta;
	state: "idle" | "loading" | "loaded" | "error";
	rows: Row[];
	/** The bundle verbatim — what the combined export embeds and what a raw view would show. */
	text?: string;
}

/**
 * The uploaded client bundles: the list, and the text of the ones the operator has opened.
 *
 * Bundles load **on demand**. Each is up to 1 MiB of a device's newest ~4096 lines, and pulling
 * every one on every visit to the troubleshooting page would cost far more than it earns — most
 * visits are about the host. Clicking a device's chip is the request to merge it in.
 */
export const useDeviceLogs = () => {
	const list = useClientLogsList();
	const [fetched, setFetched] = useState<
		Record<string, { state: DeviceBundle["state"]; rows: Row[]; text?: string }>
	>({});

	const metas = useMemo(() => list.data ?? [], [list.data]);

	const load = useCallback(async (meta: ClientLogMeta) => {
		setFetched((prev) =>
			prev[meta.id]?.state === "loaded"
				? prev
				: { ...prev, [meta.id]: { state: "loading", rows: [] } },
		);
		try {
			const text = await clientLogsGet(meta.id);
			setFetched((prev) => ({
				...prev,
				[meta.id]: { state: "loaded", rows: bundleRows(text, meta), text },
			}));
			return text;
		} catch {
			setFetched((prev) => ({
				...prev,
				[meta.id]: { state: "error", rows: [] },
			}));
			return null;
		}
	}, []);

	/**
	 * Every bundle's text, fetching whatever is not in hand yet — what the combined export needs.
	 * Sequential rather than parallel: bundles are capped at 1 MiB each and this runs behind an
	 * explicit click, so being polite to the host beats shaving a second off a rare action.
	 */
	const loadAll = useCallback(async (): Promise<
		{ meta: ClientLogMeta; text: string }[]
	> => {
		const out: { meta: ClientLogMeta; text: string }[] = [];
		for (const meta of metas) {
			const text = await load(meta);
			if (text !== null) out.push({ meta, text });
		}
		return out;
	}, [metas, load]);

	const bundles = useMemo<DeviceBundle[]>(
		() =>
			metas.map((meta) => ({
				meta,
				state: fetched[meta.id]?.state ?? "idle",
				rows: fetched[meta.id]?.rows ?? [],
				text: fetched[meta.id]?.text,
			})),
		[metas, fetched],
	);

	// A deleted bundle must not keep its rows in the viewer; dropping the fetched copy is enough,
	// since `bundles` is derived from the server list.
	const forget = useCallback((id: string) => {
		setFetched((prev) => {
			if (!(id in prev)) return prev;
			const { [id]: _gone, ...rest } = prev;
			return rest;
		});
	}, []);

	return {
		bundles,
		load,
		loadAll,
		forget,
		list: list as Loadable<ClientLogMeta[]>,
	};
};
