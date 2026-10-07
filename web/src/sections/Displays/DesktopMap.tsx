// The host's desktop, to scale (design/web-console-structure-2026-10.md §5.4).
//
// The map draws; the Screens rows act. Idle, a dashed ghost shows where the NEXT device's screen
// lands and which monitors dim — the five-second answer without a hover. Dragging a streamed
// screen arranges it.
//
// Positions come from the host in DESKTOP pixels and are rendered as percentages of the
// bounding box, so the map is responsive with no measurement. Only dragging needs the
// container's real rect, and it reads it at drag time.
import { Monitor, Settings2 } from "lucide-react";
import {
	type FC,
	type PointerEvent as ReactPointerEvent,
	useEffect,
	useRef,
	useState,
} from "react";
import type { ApiDisplayInfo, ApiMonitorInfo, Topology } from "@/api/gen/model";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages";

/** `2560x1440@120` / `2560x1440` → pixels. A head with no parsable mode is skipped. */
export function parseMode(mode: string): { w: number; h: number } | undefined {
	const hit = /^(\d+)x(\d+)/.exec(mode.trim());
	if (!hit) return undefined;
	const w = Number(hit[1]);
	const h = Number(hit[2]);
	return w > 0 && h > 0 ? { w, h } : undefined;
}

export interface MapBox {
	key: string;
	kind: "monitor" | "virtual" | "ghost";
	x: number;
	y: number;
	w: number;
	h: number;
	title: string;
	/** Mode line, or the state chip for a virtual display. */
	detail: string;
	primary?: boolean;
	/** `active` | `lingering` | `pinned` — drives the chip on a virtual box. */
	state?: string;
	/** Milliseconds until a kept display is torn down; absent when active or held. */
	expiresInMs?: number | null;
	slot?: number;
	connector?: string;
	/** This device pins something of its own, so its box is not the host's policy. */
	overlaid?: boolean;
	/** How far the map moved this box off its reported spot; a drag commits without it. */
	shift?: { x: number; y: number };
	/** Dimmed: this monitor turns off while streaming under the previewed topology. */
	dimmed?: boolean;
	draggable?: boolean;
}

/** Everything the map draws, in desktop pixels — pure, so the layout maths is testable. */
export function toBoxes(
	monitors: readonly ApiMonitorInfo[],
	displays: readonly ApiDisplayInfo[],
	opts: {
		dimMonitors?: boolean;
		/** Monitors kept lit through a stream that turns the others off. */
		keepLit?: readonly string[];
		/** Fingerprints with a stored overlay (§6.2) — their boxes carry a marker. */
		overlaid?: readonly string[];
	} = {},
): MapBox[] {
	const kept = new Set((opts.keepLit ?? []).map((c) => c.toLowerCase()));
	const boxes: MapBox[] = [];
	for (const mon of monitors) {
		// A managed head IS one of our virtual displays; drawing it twice would double every
		// streaming screen on a KWin host.
		if (mon.managed) continue;
		const size = parseMode(mon.mode);
		if (!size) continue;
		boxes.push({
			key: `mon-${mon.connector}`,
			kind: "monitor",
			x: mon.x,
			y: mon.y,
			w: size.w,
			h: size.h,
			title: mon.connector,
			detail: mon.mode,
			primary: mon.primary,
			connector: mon.connector,
			dimmed:
				(opts.dimMonitors === true && !kept.has(mon.connector.toLowerCase())) ||
				!mon.enabled,
		});
	}
	// Each group reports positions in its own space, from (0, 0). A group that would land on
	// something drawn moves right of it all: a separate desktop past a gap, an extension flush.
	// Only the live group that takes the screen stays over the monitors: that overlap is the point.
	const groups = new Map<number, ApiDisplayInfo[]>();
	for (const d of displays)
		groups.set(d.group, [...(groups.get(d.group) ?? []), d]);
	const live = (g: ApiDisplayInfo[]) =>
		g.some((d) => d.state === "active") ? 0 : 1;
	let overlaid = false;
	for (const [, group] of [...groups.entries()].sort(
		([ga, a], [gb, b]) => live(a) - live(b) || ga - gb,
	)) {
		const sized = group.flatMap((d) => {
			const size = parseMode(d.mode);
			return size ? [{ d, ...size }] : [];
		});
		if (sized.length === 0) continue;
		const rect = bounds(sized.map(({ d, w, h }) => ({ x: d.x, y: d.y, w, h })));
		const hit = boxes.filter((b) => meets(b, rect));
		const extend = group.every((d) => d.topology === "extend");
		const over = !extend && !overlaid && hit.every((b) => b.kind === "monitor");
		let shift = { x: 0, y: 0 };
		if (hit.length > 0 && !over) {
			const all = bounds(boxes);
			const gap = extend ? 0 : Math.round(rect.w / 20);
			shift = {
				x: all.minX + all.w + gap - rect.minX,
				y: all.minY - rect.minY,
			};
		}
		if (!extend && hit.length > 0 && over) overlaid = true;
		for (const { d, w, h } of sized)
			boxes.push({
				key: `slot-${d.slot}`,
				kind: "virtual",
				x: d.x + shift.x,
				y: d.y + shift.y,
				w,
				h,
				shift,
				title: d.client ?? m.display_map_unnamed(),
				detail: d.mode,
				state: d.state,
				expiresInMs: d.expires_in_ms,
				slot: d.slot,
				// By NAME: `/display/state` identifies a device the way the box labels it,
				// while overlays are keyed by fingerprint. The page resolves one to the
				// other through the paired-device list, which is the only place both live.
				overlaid: d.client != null && opts.overlaid?.includes(d.client),
				// Only a display with a stable identity slot has a manual-layout key; an anonymous
				// one has nowhere to store a position, so it cannot be arranged.
				draggable: d.identity_slot != null,
			});
	}
	return boxes;
}

/**
 * Where the next device's screen lands under `topology` (concrete, never `auto`): beside the
 * monitors, or over the main one. Sized like the main monitor — the device's own mode is not known
 * until it connects. None while a monitor is mirrored: that monitor is the screen.
 */
export function ghostBox(
	monitors: readonly ApiMonitorInfo[],
	topology: Topology,
	mirrored: boolean,
): MapBox | undefined {
	if (mirrored || topology === "auto") return undefined;
	const heads = monitors.filter((mon) => !mon.managed && mon.enabled);
	const main = heads.find((mon) => mon.primary) ?? heads[0];
	const size = (main && parseMode(main.mode)) ?? { w: 1920, h: 1080 };
	const base = {
		key: "ghost",
		kind: "ghost" as const,
		title: m.display_next_device(),
		detail: "",
		...size,
	};
	if (!main) return { ...base, x: 0, y: 0 };
	if (topology === "extend") {
		const right = Math.max(
			...heads.map((mon) => mon.x + (parseMode(mon.mode)?.w ?? 0)),
		);
		return { ...base, x: right, y: main.y };
	}
	return { ...base, x: main.x, y: main.y };
}

/** Do two rects share a pixel? Edge to edge is not overlap. */
function meets(
	a: { x: number; y: number; w: number; h: number },
	b: { minX: number; minY: number; w: number; h: number },
) {
	return (
		a.x < b.minX + b.w &&
		b.minX < a.x + a.w &&
		a.y < b.minY + b.h &&
		b.minY < a.y + a.h
	);
}

/** Bounding box over every drawn box, in desktop pixels. */
export function bounds(
	boxes: readonly { x: number; y: number; w: number; h: number }[],
) {
	const minX = Math.min(...boxes.map((b) => b.x));
	const minY = Math.min(...boxes.map((b) => b.y));
	const maxX = Math.max(...boxes.map((b) => b.x + b.w));
	const maxY = Math.max(...boxes.map((b) => b.y + b.h));
	return { minX, minY, w: maxX - minX, h: maxY - minY };
}

/**
 * Snap a dragged edge to a neighbour's edge, so screens end up flush instead of one pixel apart.
 * Tolerance is in desktop pixels and scales with the map, or snapping would be unreachable on a
 * 5K desktop drawn 600 px wide.
 */
export function snap(
	value: number,
	size: number,
	edges: readonly number[],
	tolerance: number,
): number {
	let best = value;
	let bestDelta = tolerance;
	for (const edge of edges) {
		for (const candidate of [edge, edge - size]) {
			const delta = Math.abs(candidate - value);
			if (delta < bestDelta) {
				best = candidate;
				bestDelta = delta;
			}
		}
	}
	return best;
}

/** Below this a box is too small to read or grab, so the map steps aside for the rows. */
const MIN_BOX_PX = 44;
/** The map's height at most: one 16:9 screen no longer fills a desktop's width. */
const MAX_MAP_PX = 288;

/** The element's rendered width, kept current. */
function useWidth() {
	const ref = useRef<HTMLDivElement>(null);
	const [width, setWidth] = useState(0);
	useEffect(() => {
		const el = ref.current;
		if (!el) return;
		const ro = new ResizeObserver(([entry]) =>
			setWidth(entry?.contentRect.width ?? 0),
		);
		ro.observe(el);
		return () => ro.disconnect();
	}, []);
	return [ref, width] as const;
}

export const DesktopMap: FC<{
	monitors: readonly ApiMonitorInfo[];
	displays: readonly ApiDisplayInfo[];
	/** Preview: the selected policy turns the physical monitors off while streaming. */
	dimMonitors?: boolean;
	keepLit?: readonly string[];
	/** Where the next device's screen lands; drawn only while nothing streams. */
	ghost?: MapBox;
	/** Devices that pin settings of their own, by the name their box carries. */
	overlaid?: readonly string[];
	/** Pinned monitor (`capture_monitor`), so its box can show it is the streamed one. */
	captureMonitor?: string | null;
	/** Commit a dragged position, in desktop pixels. */
	onMove?: (slot: number, x: number, y: number) => void;
	busy?: boolean;
}> = ({
	monitors,
	displays,
	dimMonitors,
	keepLit,
	ghost,
	overlaid,
	captureMonitor,
	onMove,
	busy,
}) => {
	const [measure, width] = useWidth();
	// While a box is being dragged its position is local; everything else still comes from the
	// host, so a poll landing mid-drag cannot yank the box out from under the pointer.
	const [drag, setDrag] = useState<{
		slot: number;
		x: number;
		y: number;
	} | null>(null);

	const drawn = toBoxes(monitors, displays, { dimMonitors, keepLit, overlaid });
	const boxes = ghost && displays.length === 0 ? [...drawn, ghost] : drawn;
	const placed = boxes.map((b) =>
		drag && b.slot === drag.slot ? { ...b, x: drag.x, y: drag.y } : b,
	);
	const box = bounds(placed);
	const smallest = Math.min(...placed.map((b) => b.w));
	const mapWidth = Math.min(width, (MAX_MAP_PX * box.w) / box.h);
	const readable =
		placed.length > 0 &&
		(width === 0 || (smallest / box.w) * mapWidth >= MIN_BOX_PX);
	const pct = (v: number, span: number) => `${(v / span) * 100}%`;

	const onPointerDown = (b: MapBox) => (e: ReactPointerEvent<HTMLElement>) => {
		if (!onMove || !b.draggable || b.slot === undefined || busy) return;
		const container = e.currentTarget.parentElement;
		if (!container) return;
		const rect = container.getBoundingClientRect();
		if (rect.width === 0) return;
		// Desktop pixels per screen pixel, read fresh on every move: dragging a screen past
		// the current edge widens the bounding box, the container re-scales, and a ratio
		// captured once would let the box drift away from the cursor for the rest of the drag.
		const ratio = () => {
			const live = container.getBoundingClientRect().width;
			return live > 0 ? box.w / live : box.w / rect.width;
		};
		const scale = ratio();
		const grabX = e.clientX * scale - b.x;
		const grabY = e.clientY * scale - b.y;
		// Every other box's edges are what a drag snaps to.
		const xEdges = placed
			.filter((o) => o.key !== b.key)
			.flatMap((o) => [o.x, o.x + o.w]);
		const yEdges = placed
			.filter((o) => o.key !== b.key)
			.flatMap((o) => [o.y, o.y + o.h]);
		const tolerance = Math.max(8, box.w * 0.02);
		e.currentTarget.setPointerCapture(e.pointerId);

		const move = (ev: PointerEvent) => {
			const s = ratio();
			setDrag({
				slot: b.slot as number,
				x: Math.round(snap(ev.clientX * s - grabX, b.w, xEdges, tolerance)),
				y: Math.round(snap(ev.clientY * s - grabY, b.h, yEdges, tolerance)),
			});
		};
		const up = () => {
			window.removeEventListener("pointermove", move);
			window.removeEventListener("pointerup", up);
			// Read the committed position from state at drop time rather than closing over a
			// stale one.
			setDrag((d) => {
				if (d && d.slot === b.slot && (d.x !== b.x || d.y !== b.y)) {
					onMove(d.slot, d.x - (b.shift?.x ?? 0), d.y - (b.shift?.y ?? 0));
				}
				return null;
			});
		};
		window.addEventListener("pointermove", move);
		window.addEventListener("pointerup", up);
	};

	return (
		<div ref={measure} className="w-full">
			{readable && (
				<div
					// The desktop's own proportions, as wide as the card or MAX_MAP_PX tall.
					className="relative mx-auto overflow-hidden rounded-lg border bg-muted/30"
					style={{
						aspectRatio: `${box.w} / ${box.h}`,
						width: `min(100%, ${(MAX_MAP_PX * box.w) / box.h}px)`,
					}}
					role="img"
					aria-label={m.display_map_label()}
				>
					{placed.map((b) => (
						// The box is a drag handle, not a control: every action it offers is also on the
						// rows below the map, which are the keyboard and screen-reader path.
						<div
							key={b.key}
							onPointerDown={onPointerDown(b)}
							className={cn(
								"absolute flex flex-col justify-between gap-1 overflow-hidden rounded-md border p-1.5 text-[10px] leading-tight sm:p-2 sm:text-xs",
								b.kind === "virtual"
									? "border-dashed border-primary/70 bg-[color-mix(in_oklab,var(--primary)_12%,var(--card))]"
									: b.kind === "ghost"
										? "border-dashed border-muted-foreground/60 bg-transparent text-muted-foreground"
										: "border-border bg-card",
								// A streamed screen sits ON the desktop, so it draws over the heads it
								// covers — under `primary` and `exclusive` it shares their origin, and
								// relying on source order left its name hidden under a monitor's box.
								b.kind !== "monitor" && "z-[1]",
								b.dimmed && "opacity-40",
								b.draggable &&
									onMove &&
									"cursor-grab touch-none active:cursor-grabbing",
								drag != null &&
									drag.slot === b.slot &&
									"z-10 ring-2 ring-primary",
							)}
							style={{
								left: pct(b.x - box.minX, box.w),
								top: pct(b.y - box.minY, box.h),
								width: pct(b.w, box.w),
								height: pct(b.h, box.h),
							}}
						>
							<div className="min-w-0">
								<div className="flex items-center gap-1">
									<span className="truncate font-medium">{b.title}</span>
									{b.primary && (
										<span role="img" aria-label={m.display_monitor_primary()}>
											★
										</span>
									)}
									{b.connector && b.connector === captureMonitor && (
										<Monitor
											className="size-3 shrink-0"
											aria-label={m.display_map_streamed()}
										/>
									)}
									{b.overlaid && (
										<Settings2
											className="size-3 shrink-0"
											aria-label={m.display_device_settings()}
										/>
									)}
								</div>
								<div className="truncate text-muted-foreground">{b.detail}</div>
							</div>
							{b.kind === "virtual" && (
								<span className="truncate text-muted-foreground">
									{stateLabel(b.state, b.expiresInMs)}
								</span>
							)}
						</div>
					))}
				</div>
			)}
		</div>
	);
};

/** Outcomes, not the wire's words (D10): never "Lingering" or "Pinned". */
export function stateLabel(
	state?: string,
	expiresInMs?: number | null,
): string {
	switch (state) {
		case "active":
			return m.display_state_streaming();
		case "pinned":
			return m.display_state_kept_until();
		case "lingering":
			// The countdown is the reason this box is still on the map, so it rides the chip
			// rather than being dropped with the list that used to carry it. Rounded up: "0 s"
			// on a display that has not gone yet reads as a bug.
			return expiresInMs == null
				? m.display_state_kept()
				: m.display_state_kept_for({ seconds: Math.ceil(expiresInMs / 1000) });
		default:
			return "";
	}
}
