// What this host has been doing — the event stream, rendered.
//
// The console could describe the present (a status snapshot) but never the recent past: a client
// that connected and left while you were on another page left no trace anywhere you could look.
// The stream was already open for cache invalidation, so this costs one ring buffer.
//
// In-memory and bounded, so it starts empty on a page load and fills as things happen. That is the
// honest shape for a live tail — pretending to be a durable log would need the host to keep one.
//
// Home's *Recent*: the newest handful, and Show all expands the whole ring in place. All 200 at
// once pushed everything below off the page on a busy host.

import { Activity as ActivityIcon, ChevronDown } from "lucide-react";
import { AnimatePresence, motion } from "motion/react";
import { type FC, useState } from "react";
import {
	type ActivityEntry,
	useActivity,
	useActivityReady,
} from "@/api/events";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { eventKindLabel } from "@/lib/event-kinds";
import { fmtDateTime } from "@/lib/format";
import { m } from "@/paraglide/messages";

/** How many rows the card shows folded. The rest are a click away, not gone. */
const CARD_MAX = 6;

/** Tighter than the house 0.1 s: a dozen short rows at 0.1 s took over a second to arrive. */
const ROW_GAP = 0.035;

/** How many rows still step. Past this they share the last one's delay, or a 200-entry ring on
 *  the full page would take seconds to finish arriving. */
const STAGGER_STEPS = 10;

/**
 * How long the rows wait after their card starts arriving. They fade in under the card's own fade,
 * so rows starting with it spent the first half of their cascade on a card nobody could see yet.
 */
const CARD_LEAD = 0.18;

/** When row `i` of the batch a list mounts with starts, counted from its card's start. */
const rowDelay = (i: number) =>
	CARD_LEAD + Math.min(i, STAGGER_STEPS) * ROW_GAP;

/**
 * A row's arrival. Sliding from above is the direction the list grows: a new event is prepended,
 * so it comes in over the row it displaced.
 *
 * No delay here, and none on the row's own `transition` — that one also drives its `layout` slide,
 * and every arrival re-indexes the survivors. The list's `delayChildren` times the batch it mounts
 * with; anything later lands at once.
 */
const ROW_VARIANTS = {
	from: { opacity: 0, y: -10 },
	enter: { opacity: 1, y: 0 },
};

/**
 * A row leaving. An object, never a variant label: a label on `initial`, `animate` or `exit` makes
 * the row drive its own variants, so it stops inheriting `from → enter` — the rows then mounted at
 * full opacity with no entrance at all.
 */
const ROW_EXIT = { opacity: 0, y: 6 };

/**
 * The feed itself.
 *
 * `AnimatePresence` is what makes this readable while it is live: without it a row pushed past
 * the cap by a newer event simply vanished mid-glance, and `layout` carries the survivors down
 * rather than snapping them.
 *
 * The rows inherit `from → enter` through the list from the card, so the cascade is timed from the
 * moment the card starts — whatever delayed it: its slot in the page's stagger on a navigation, the
 * replay on a reload. That holds because the card mounts in the same render as its rows
 * (`useActivityReady`). A row mounting into a list already on screen animates by itself, at once,
 * which is what a live arrival should do.
 */
/** An entry, with how many identical ones followed it in a row. */
export type ActivityRow = ActivityEntry & { count: number };

/**
 * Consecutive events of one kind that say the same thing fold into one row: a sync that fires
 * `library.changed` six times in a second is one line, not six.
 */
export function collapse(entries: ActivityEntry[]): ActivityRow[] {
	const out: ActivityRow[] = [];
	for (const e of entries) {
		const last = out[out.length - 1];
		if (last && last.kind === e.kind && describe(last) === describe(e))
			last.count += 1;
		else out.push({ ...e, count: 1 });
	}
	return out;
}

export const ActivityList: FC<{ entries: ActivityRow[] }> = ({ entries }) => (
	<motion.ul
		variants={{ from: {}, enter: {} }}
		transition={{ delayChildren: rowDelay }}
		className="flex flex-col divide-y"
	>
		<AnimatePresence>
			{entries.map((e) => (
				<motion.li
					key={e.seq}
					layout
					exit={ROW_EXIT}
					variants={ROW_VARIANTS}
					className="flex flex-wrap items-center gap-x-3 gap-y-1 py-2 first:pt-0 last:pb-0"
				>
					<Badge variant={toneFor(e.kind)}>{eventKindLabel(e.kind)}</Badge>
					{e.count > 1 && (
						<span className="text-xs tabular-nums text-muted-foreground">
							×{e.count}
						</span>
					)}
					<span className="min-w-0 flex-1 truncate text-sm">{describe(e)}</span>
					<time
						dateTime={new Date(e.ts_ms).toISOString()}
						className="shrink-0 text-xs tabular-nums text-muted-foreground"
					>
						{fmtDateTime(e.ts_ms)}
					</time>
				</motion.li>
			))}
		</AnimatePresence>
	</motion.ul>
);

/** Home's card: the newest few, with the rest folded under Show all. */
export const ActivityCard: FC = () => {
	const entries = useActivity();
	const ready = useActivityReady();
	// Not until the feed is settled: a card that mounts empty and fills a moment later animates on
	// a different path each time. Mounted with its rows, it animates the same way every time.
	if (!ready) return null;
	return <ActivityCardView entries={entries} />;
};

/** Split from the hook so a story can hand it a busy host — the cap and the cadence are the
 *  parts worth looking at, and neither shows up without one. */
export const ActivityCardView: FC<{ entries: ActivityEntry[] }> = ({
	entries,
}) => {
	const [all, setAll] = useState(false);
	const rows = collapse(entries);
	return (
		<Card>
			<CardHeader>
				<CardTitle className="flex items-center gap-2">
					<ActivityIcon className="size-4" />
					{m.activity_title()}
				</CardTitle>
			</CardHeader>
			<CardContent className="space-y-3">
				{entries.length === 0 ? (
					<p className="text-sm text-muted-foreground">{m.activity_empty()}</p>
				) : (
					<>
						<ActivityList entries={all ? rows : rows.slice(0, CARD_MAX)} />
						{all ? (
							// The ring is per page load; a reader at its bottom has earned that fact.
							<p className="border-t pt-3 text-xs text-muted-foreground">
								{m.activity_ring_note()}
							</p>
						) : (
							rows.length > CARD_MAX && (
								<div className="flex justify-end border-t pt-3">
									<Button
										variant="ghost"
										size="sm"
										onClick={() => setAll(true)}
									>
										{m.activity_show_all()}
										<ChevronDown className="size-4" />
									</Button>
								</div>
							)
						)}
					</>
				)}
			</CardContent>
		</Card>
	);
};

/** The subject of an event, in one line — whatever the payload actually names. */
export function describe(e: ActivityEntry): string {
	const d = e.data;
	const client =
		pick(d.client, "name") ??
		pick(d.session, "client") ??
		pick(d.game, "client") ??
		pick(d.device, "name");
	const stream = d.stream as Record<string, unknown> | undefined;
	const parts = [
		client,
		typeof stream?.app === "string" ? stream.app : undefined,
		pick(d.game, "title"),
		typeof d.reason === "string" ? d.reason : undefined,
	].filter((x): x is string => typeof x === "string" && x.length > 0);
	// An event whose payload names nothing (host.started, library.changed) is still worth a row —
	// the kind badge carries the whole meaning, so leave the line blank rather than inventing text.
	return parts.join(" · ");
}

/** Read a string field off a nested ref object, tolerating anything unexpected. */
function pick(obj: unknown, key: string): string | undefined {
	if (!obj || typeof obj !== "object") return undefined;
	const v = (obj as Record<string, unknown>)[key];
	if (typeof v === "string") return v;
	// `SessionRef.client` is itself a ClientRef.
	if (v && typeof v === "object") {
		const name = (v as Record<string, unknown>).name;
		return typeof name === "string" ? name : undefined;
	}
	return undefined;
}

/** Colour by what the event means, not by its domain — good news green, losses muted, denials red. */
function toneFor(
	kind: string,
): "success" | "destructive" | "secondary" | "outline" {
	if (kind === "pairing.denied") return "destructive";
	if (kind.endsWith(".connected") || kind.endsWith(".started"))
		return "success";
	if (kind === "pairing.completed") return "success";
	if (kind.endsWith(".disconnected") || kind.endsWith(".ended"))
		return "outline";
	if (kind.endsWith(".stopped") || kind.endsWith(".exited")) return "outline";
	return "secondary";
}
