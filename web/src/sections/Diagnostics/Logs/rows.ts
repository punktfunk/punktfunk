import type { ClientLogMeta } from "@/api/gen/model/clientLogMeta";
import type { LogEntry } from "@/api/gen/model/logEntry";

/**
 * One row model for every producer, so the viewer has a single timeline instead of a stream plus a
 * table of attachments.
 *
 * The insight this rests on: a client bundle is not a foreign artifact. `clients/session`'s
 * `ring_layer` formats each line as `<ISO8601-Z> <LEVEL> <target> <msg>` — the same four fields as
 * a host `LogEntry`, only serialized as text instead of JSON — and it does so in **wall clock**
 * precisely "so a bundle correlates with the host log it lands next to". Parsing it back into rows
 * is therefore recovering structure the client already had, not inventing it, and it buys the view
 * the whole feature exists for: the client's stall and the host's account of the same second, on
 * one screen.
 */

/** Source ids. Devices get one each, so a chip can address a single bundle. */
export const HOST_SOURCE = "host";
export const PLUGINS_SOURCE = "plugins";
export const deviceSource = (bundleId: string): string => `device:${bundleId}`;

/** The target prefix the host stamps on every runner-shipped line. */
const PLUGIN_TARGET_PREFIX = "plugin:";

export interface Row {
	/**
	 * React key. `seq` is unique within the host ring but says nothing about a bundle's lines, so
	 * the moment one is loaded two rows collide and React silently drops one. Always compose the
	 * source into the key.
	 */
	key: string;
	/**
	 * Wall-clock ms. Host rows carry the host's clock, device rows the device's — the two can be
	 * minutes apart, which is exactly why every device row is tagged with where it came from.
	 */
	ts: number;
	level: string;
	target: string;
	msg: string;
	source: string;
	/**
	 * The device a row came from, rendered as the origin tag. Host and plugin rows carry none:
	 * absence reads as "this host", and tagging every host line would double the noise in the
	 * common case where no bundle is loaded at all.
	 */
	device?: string;
}

/** Host ring entries, split onto the two producers the ring interleaves. */
export const hostRows = (entries: LogEntry[]): Row[] =>
	entries.map((e) => {
		const source = e.target.startsWith(PLUGIN_TARGET_PREFIX)
			? PLUGINS_SOURCE
			: HOST_SOURCE;
		return {
			key: `${source}:${e.seq}`,
			ts: e.ts_ms,
			level: e.level,
			target: e.target,
			msg: e.msg,
			source,
		};
	});

/**
 * `2026-08-15T12:03:47.123Z INFO  punktfunk_session::stream  frame late by 34ms`
 *
 * The level is written through `{:5}`, so it arrives padded and the gap before the target is one
 * or two spaces — matched loosely rather than pinned, since the padding is a formatting detail of
 * the client and not a wire contract.
 */
const BUNDLE_LINE =
	/^(\d{4}-\d{2}-\d{2}T[\d:.]+Z)\s+([A-Z]{4,5})\s+(\S+)\s*([\s\S]*)$/;

/**
 * A bundle's text as rows.
 *
 * **Fails soft, by design.** Only the desktop session shell installs the ring layer today; the
 * Apple, Android and webOS legs are still open and will not emit this shape when they land. A line
 * that does not parse is therefore kept verbatim as its own row rather than dropped — an
 * unrecognized format degrades to "a log you can still read and search", never to a blank pane.
 * The same path carries the bundle's header line and its `… N older lines evicted …` note, which
 * are prose and were never meant to parse.
 */
export const bundleRows = (text: string, meta: ClientLogMeta): Row[] => {
	const source = deviceSource(meta.id);
	const rows: Row[] = [];
	// Unparsed lines inherit the timestamp of the line above so they sort beside it. Those that
	// arrive BEFORE any timestamped line (the bundle header) have nothing to inherit yet and are
	// backfilled from the first real one below — otherwise the header sorts to 1970 and the merged
	// view opens on it.
	const leading: Row[] = [];
	let lastTs: number | null = null;

	text.split("\n").forEach((line, i) => {
		if (line.trim() === "") return;
		const key = `${source}:${i}`;
		const [, stamp, level, target, msg] = BUNDLE_LINE.exec(line) ?? [];
		const ts = stamp === undefined ? Number.NaN : Date.parse(stamp);

		if (level !== undefined && target !== undefined && !Number.isNaN(ts)) {
			lastTs = ts;
			rows.push({
				key,
				ts,
				level,
				target,
				msg: msg ?? "",
				source,
				device: meta.device_name,
			});
			return;
		}

		const row: Row = {
			key,
			ts: lastTs ?? 0,
			level: "",
			target: "",
			msg: line,
			source,
			device: meta.device_name,
		};
		rows.push(row);
		if (lastTs === null) leading.push(row);
	});

	// `received_ms` is the fallback for a bundle that parsed nothing at all: it is the one
	// timestamp we always have, and it puts such a bundle at the point in the timeline where it
	// actually arrived rather than at the epoch.
	const firstTs = rows.find((r) => r.ts > 0)?.ts ?? meta.received_ms;
	for (const row of leading) row.ts = firstTs;

	return rows;
};

/**
 * Merge pre-sorted groups onto one timeline.
 *
 * `sort` is stable (spec-required since ES2019), so rows sharing a millisecond keep their own
 * source's order instead of shuffling between polls.
 */
export const mergeRows = (groups: Row[][]): Row[] =>
	groups.length === 1
		? (groups[0] ?? [])
		: groups.flat().sort((a, b) => a.ts - b.ts);
