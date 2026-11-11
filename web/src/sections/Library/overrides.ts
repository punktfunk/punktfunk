// The words and arithmetic of game overrides, without React: option lists, the badge's summary,
// how many entries name a shared override, and the entry an edit writes.
import type { GameOverrides } from "@/api/gen/model/gameOverrides";
import type { OverrideEntry } from "@/api/gen/model/overrideEntry";
import type { OverridesDoc } from "@/api/gen/model/overridesDoc";
import { m } from "@/paraglide/messages";
import { SETTING_COPY } from "../HostSettings/copy";

/** The four values and the screen option, as a game, a source or a shared override holds them. */
export type Fields = Omit<GameOverrides, "name">;
/** A field edit: `null` clears the field. */
export type Patch = { [K in keyof Fields]?: NonNullable<Fields[K]> | null };

export const RESOLUTIONS = [2160, 1440, 1080, 720] as const;
export const REFRESH_RATES = [144, 120, 90, 60, 30] as const;
/** What the host takes for a custom value: `MIN_LINES`… in `game_overrides.rs`. */
export const LINES_RANGE = [360, 4320] as const;
export const HZ_RANGE = [24, 240] as const;

/** A typed custom value as a whole number clamped to `range`; null when it isn't a number. */
export function wholeIn(
	raw: string,
	[min, max]: readonly [number, number],
): number | null {
	const n = Math.round(Number(raw));
	if (raw.trim() === "" || !Number.isFinite(n)) return null;
	return Math.min(max, Math.max(min, n));
}

/** `2160` reads `4K`; any other height is its `p` name. */
export const linesLabel = (lines: number): string =>
	lines === 2160 ? "4K" : `${lines}p`;

/** The pad's name as Host settings words it; a name this console doesn't know stays itself. */
export function padLabel(name: string): string {
	return SETTING_COPY.gamepad?.options?.[name]?.() ?? name;
}

/** Every pad name the host takes, in the order Host settings lists them. */
export const padNames = (): string[] =>
	Object.keys(SETTING_COPY.gamepad?.options ?? {});

/** The library id's source, as the host splits it: `heroic:legendary:x` is `heroic`. */
export const storeOf = (id: string): string => {
	const at = id.indexOf(":");
	return at < 0 ? id : id.slice(0, at);
};

/** The values alone, without a set id, a name, nulls or an option that does nothing. */
export function fieldsOf(o: OverrideEntry | GameOverrides | undefined): Fields {
	const out: Fields = {};
	if (o?.lines != null) out.lines = o.lines;
	if (o?.hz != null) out.hz = o.hz;
	if (o?.hdr != null) out.hdr = o.hdr;
	if (o?.gamepad != null) out.gamepad = o.gamepad;
	if (o?.beyond_screen && out.lines != null) out.beyond_screen = true;
	return out;
}

/** `base` with `patch` applied, as the entry to write. Empty means none. */
export const applyPatch = (base: Fields, patch: Patch): Fields =>
	fieldsOf({ ...base, ...patch } as GameOverrides);

/**
 * What game `id` stores for `data`; `null` follows its source again. Nothing of its own under a
 * source's entry is `none`, or the game would take the source's.
 */
export function gameEntry(
	doc: OverridesDoc | undefined,
	id: string,
	data: OverrideEntry | null,
): OverrideEntry {
	if (!data) return {};
	const empty = !data.set && Object.keys(fieldsOf(data)).length === 0;
	return empty && doc?.sources?.[storeOf(id)] ? { none: true } : data;
}

/**
 * The badge: the shared override's name, else the values in a few words, `1080p60 · HDR off ·
 * DualSense`. Null when nothing is set.
 */
export function streamSummary(
	o: GameOverrides | null | undefined,
): string | null {
	if (!o) return null;
	if (o.name) return o.name;
	const parts: string[] = [];
	if (o.lines != null)
		parts.push(`${linesLabel(o.lines)}${o.hz != null ? o.hz : ""}`);
	else if (o.hz != null) parts.push(m.library_stream_hz({ hz: o.hz }));
	if (o.hdr != null)
		parts.push(o.hdr ? m.library_stream_hdr_on() : m.library_stream_hdr_off());
	if (o.gamepad) parts.push(padLabel(o.gamepad));
	return parts.length ? parts.join(" · ") : null;
}

/** The shared overrides, by name in `locale`'s order. */
export const setList = (
	doc: OverridesDoc | undefined,
	locale: string,
): (GameOverrides & { id: string })[] =>
	Object.entries(doc?.sets ?? {})
		.map(([id, s]) => ({ ...s, id }))
		.sort((a, b) => (a.name ?? "").localeCompare(b.name ?? "", locale));

/** How many games and sources name shared override `set`. */
export function usageOf(
	doc: OverridesDoc | undefined,
	set: string,
): { games: number; sources: number } {
	const count = (entries: Record<string, OverrideEntry> | undefined) =>
		Object.values(entries ?? {}).filter((e) => e.set === set).length;
	return { games: count(doc?.games), sources: count(doc?.sources) };
}
