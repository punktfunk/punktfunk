// The parity harness: proof that a library plugin reproduces the built-in scanner it replaced.
//
// This is the acceptance gate for every extracted scanner
// (library-scanner-plugins-implementation-plan.md M5). Ported unit tests are
// necessary but nowhere near sufficient — they pin the PARSERS, while what actually has to hold is
// that the whole pipeline lands the same entries, with the same ids, launch recipes and detect
// signals, on a real box with a real launcher installed. A plugin that parses perfectly and emits
// `steam:440` as `steam:440.0` breaks every Moonlight pin on the host and no parser test notices.
//
// It lives in the KIT, not in a plugin, because it is identical for every scanner: capture what a
// host reports while its built-in scanner is doing the work, then check the plugin produces the
// same set.
// (One plugin per repo is the house pattern, so anything shared has to be published, not adjacent.)
//
// Usage, per plugin, on a box with that launcher installed:
//
//   punktfunk-plugin-steam parity --snapshot before.json   # host still on its built-in scanner
//   punktfunk-plugin-steam parity --compare before.json    # runs THIS plugin's scan
//
// `--compare` runs the plugin's own scan directly rather than installing it first, so a mismatch is
// visible before anything is published — and the run is repeatable while you fix it.
import type { ProviderEntry } from "../wire.js";

/** The four art slots, in the order of the host's `ArtKind::ALL`. */
const ART_KINDS = ["portrait", "hero", "logo", "header"] as const;
type ArtKind = (typeof ART_KINDS)[number];

/** One entry, reduced to the facts parity is about. */
export interface ParityEntry {
	/** The store-qualified library id — the field everything downstream is keyed on. */
	readonly id: string;
	readonly title: string;
	/** `<kind>:<value>`, or null when the entry has no launch recipe. */
	readonly launch: string | null;
	/** `"game"` or `"launcher"`. */
	readonly role: string;
	/**
	 * Which art kinds are PRESENT, not their values. The representation legitimately changes on
	 * extraction (a scanner's `data:` URL or host-relative proxy path becomes a `file://` path or a
	 * CDN URL), so comparing values would fail every time for no reason. Presence is the invariant
	 * that matters: a title that had a poster must still have one.
	 */
	readonly art: Readonly<Record<ArtKind, boolean>>;
	/** Flat descriptive metadata (platform, genres, …) — compared verbatim. */
	readonly meta: Readonly<Record<string, unknown>>;
}

/** What the host reports for one entry in `GET /library`. */
export interface HostGameEntry {
	id: string;
	store: string;
	title: string;
	role?: string;
	launch?: { kind: string; value: string } | null;
	art?: Partial<Record<ArtKind, string | null>>;
	[extra: string]: unknown;
}

/** Keys on a host entry that are structure, not descriptive metadata. */
const NON_META = new Set([
	"id",
	"store",
	"title",
	"role",
	"launch",
	"art",
	"provider",
	"external_id",
	"prep",
	"detect",
]);

const artPresence = (
	art: Partial<Record<ArtKind, string | null>> | undefined,
): Record<ArtKind, boolean> => {
	const out = {} as Record<ArtKind, boolean>;
	for (const k of ART_KINDS) out[k] = Boolean(art?.[k]);
	return out;
};

const pickMeta = (src: Record<string, unknown>): Record<string, unknown> => {
	const out: Record<string, unknown> = {};
	for (const [k, v] of Object.entries(src)) {
		// Absent and empty are the same thing here: the host omits empty lists and null fields, and a
		// plugin that sends `genres: []` has not changed anything.
		if (NON_META.has(k) || v == null) continue;
		if (Array.isArray(v) && v.length === 0) continue;
		out[k] = v;
	}
	return out;
};

/** The library id the host assigns a claimed entry — the deterministic `<store>:<external_id>`. */
export const claimedLibraryId = (store: string, externalId: string): string =>
	`${store}:${externalId}`;

/** Reduce what the host reported (the BEFORE side) to a comparable entry. */
export const fromHostEntry = (e: HostGameEntry): ParityEntry => ({
	id: e.id,
	title: e.title,
	launch: e.launch ? `${e.launch.kind}:${e.launch.value}` : null,
	role: e.role ?? "game",
	art: artPresence(e.art),
	meta: pickMeta(e as Record<string, unknown>),
});

/** Reduce what this plugin produced (the AFTER side) to a comparable entry. */
export const fromProviderEntry = (
	store: string,
	e: ProviderEntry,
): ParityEntry => {
	const rec = e as unknown as Record<string, unknown>;
	return {
		id: claimedLibraryId(store, e.external_id),
		title: e.title,
		launch: e.launch ? `${e.launch.kind}:${e.launch.value}` : null,
		role: (e as { role?: string }).role ?? "game",
		art: artPresence(
			e.art as Partial<Record<ArtKind, string | null>> | undefined,
		),
		meta: pickMeta(rec),
	};
};

/** One field that differs between the two sides. */
export interface ParityChange {
	readonly id: string;
	readonly field: string;
	readonly before: unknown;
	readonly after: unknown;
}

export interface ParityReport {
	/** In the baseline, absent from what the plugin produced — the plugin LOST a title. */
	readonly missing: ParityEntry[];
	/** Produced by the plugin, absent from the baseline — the plugin invented a title. */
	readonly extra: ParityEntry[];
	/** Same id, different facts. */
	readonly changed: ParityChange[];
	/** Entries present on both sides and identical. */
	readonly matched: number;
	/**
	 * Launcher entries the plugin adds (design D4). Never a failure: the built-in scanner had no
	 * concept of them, so they are expected to be `extra` and are reported separately so a real
	 * regression isn't buried under them.
	 */
	readonly launchersAdded: ParityEntry[];
	readonly ok: boolean;
}

/**
 * Diff a baseline (what the host reported while its built-in scanner ran) against what this plugin
 * produced. `ok` is true only when nothing is missing, nothing unexpected is extra, and no compared
 * field changed.
 */
export const diffParity = (
	baseline: ReadonlyArray<ParityEntry>,
	produced: ReadonlyArray<ParityEntry>,
): ParityReport => {
	const byId = new Map(baseline.map((e) => [e.id, e]));
	const producedIds = new Set(produced.map((e) => e.id));
	const changed: ParityChange[] = [];
	const extra: ParityEntry[] = [];
	const launchersAdded: ParityEntry[] = [];
	let matched = 0;

	for (const after of produced) {
		const before = byId.get(after.id);
		if (!before) {
			// A launcher entry has no counterpart by construction — the scanner never emitted one.
			(after.role === "launcher" ? launchersAdded : extra).push(after);
			continue;
		}
		const diffs = compareEntry(before, after);
		if (diffs.length === 0) matched++;
		else changed.push(...diffs);
	}

	const missing = baseline.filter((e) => !producedIds.has(e.id));
	return {
		missing,
		extra,
		changed,
		matched,
		launchersAdded,
		ok: missing.length === 0 && extra.length === 0 && changed.length === 0,
	};
};

const compareEntry = (
	before: ParityEntry,
	after: ParityEntry,
): ParityChange[] => {
	const out: ParityChange[] = [];
	const note = (field: string, b: unknown, a: unknown) =>
		out.push({ id: before.id, field, before: b, after: a });

	if (before.title !== after.title) note("title", before.title, after.title);
	if (before.launch !== after.launch)
		note("launch", before.launch, after.launch);
	if (before.role !== after.role) note("role", before.role, after.role);
	for (const k of ART_KINDS) {
		// Only a LOST art kind is a regression. Gaining one is an improvement (the plugin can reach
		// art the host never resolved), and failing a run over it would just train people to ignore
		// the harness.
		if (before.art[k] && !after.art[k]) note(`art.${k}`, true, false);
	}
	const keys = new Set([
		...Object.keys(before.meta),
		...Object.keys(after.meta),
	]);
	for (const k of keys) {
		const b = before.meta[k];
		const a = after.meta[k];
		if (JSON.stringify(b) !== JSON.stringify(a)) note(`meta.${k}`, b, a);
	}
	return out;
};

/** Render a report for a terminal. Empty-ish when everything matched. */
export const formatParityReport = (r: ParityReport): string => {
	const lines: string[] = [];
	lines.push(
		r.ok
			? `parity OK — ${r.matched} entries identical`
			: `parity FAILED — ${r.matched} identical, ${r.missing.length} missing, ${r.extra.length} unexpected, ${r.changed.length} changed`,
	);
	for (const e of r.missing) lines.push(`  missing:  ${e.id}  ${e.title}`);
	for (const e of r.extra) lines.push(`  extra:    ${e.id}  ${e.title}`);
	for (const c of r.changed) {
		lines.push(
			`  changed:  ${c.id}  ${c.field}: ${JSON.stringify(c.before)} -> ${JSON.stringify(c.after)}`,
		);
	}
	if (r.launchersAdded.length > 0) {
		lines.push(
			`  (+${r.launchersAdded.length} launcher ${r.launchersAdded.length === 1 ? "entry" : "entries"}, expected: ${r.launchersAdded
				.map((e) => e.id)
				.join(", ")})`,
		);
	}
	// Art REPRESENTATION always changes on extraction (a host-relative proxy path or an inlined
	// `data:` URL becomes a `file://` path or a CDN URL). Presence is what this harness checks, so
	// say plainly that the bytes still want a human's eyes once.
	if (r.ok) {
		lines.push(
			"  note: art is compared by presence, not value — spot-check a few covers render.",
		);
	}
	return lines.join("\n");
};
