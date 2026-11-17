// The activity ring's one piece of logic, kept free of React and the event stream so it can be
// tested on its own.

/** One thing that happened, as the feed renders it. */
export interface ActivityEntry {
	/** The host's monotonic sequence number — stable, and a good React key. */
	seq: number;
	/** Unix ms, from the host's clock (never the browser's). */
	ts_ms: number;
	kind: string;
	/** The event payload, shape depending on `kind` (see the EventKind schema). */
	data: Record<string, unknown>;
}

/** How much history the ring keeps. Show all on Home expands the whole of it. */
export const ACTIVITY_MAX = 200;

/**
 * Fold a batch of frames into the ring: newest first, capped, and a seq already held dropped — a
 * reconnect re-delivers its cursor.
 *
 * Returns `held` itself when nothing is new, so the store can tell "nothing changed" by identity
 * and skip notifying, which is what keeps a duplicate-only batch from costing a render.
 */
export function mergeActivity(
	held: ActivityEntry[],
	batch: ActivityEntry[],
	max: number = ACTIVITY_MAX,
): ActivityEntry[] {
	const seen = new Set(held.map((e) => e.seq));
	const fresh: ActivityEntry[] = [];
	for (const e of batch) {
		if (seen.has(e.seq)) continue;
		seen.add(e.seq);
		fresh.push(e);
	}
	if (fresh.length === 0) return held;
	return [...fresh, ...held].sort((a, b) => b.seq - a.seq).slice(0, max);
}
