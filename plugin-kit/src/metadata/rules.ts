// The pure half of an Art & Metadata source: which entries are worth a lookup, what a verdict is
// keyed on, when it goes stale, and what a push carries. Apart from the Effect loop so the rules
// the host's merge depends on are tested without a host.
import { Schema } from "effect";
import {
	ART_KINDS,
	type ArtKind,
	Artwork,
	GameMeta,
	type MetadataEntry,
	type MetaField,
} from "../wire.js";

/** A library entry as `GET /library` lists it to a plugin: the fields a source matches on. */
export type LibraryEntry = GameMeta & {
	readonly id: string;
	readonly store: string;
	readonly title: string;
	readonly role?: "game" | "launcher";
	readonly art?: Artwork;
	readonly ids?: Readonly<Record<string, string>>;
	/** Where each borrowed value came from: slot or field → source id, or `pick`. */
	readonly filled?: Readonly<Record<string, string>>;
};

/** What a source can fill. An entry lacking none of it is not looked up. */
export interface Offers {
	readonly art?: ReadonlyArray<ArtKind>;
	readonly meta?: ReadonlyArray<MetaField>;
}

/** Where an entry sits in the source's catalog. `key` is what an operator's pin stores. */
export interface Match {
	readonly key: string;
	readonly label: string;
}

/** What a source has for a match. Art is `http(s)` URLs; anything else is dropped. */
export interface Found {
	readonly art?: Artwork;
	readonly meta?: GameMeta;
}

export const Verdict = Schema.Struct({
	/** The `matcherVersion` it was resolved under. */
	v: Schema.Number,
	identity: Schema.String,
	at: Schema.Number,
	match: Schema.NullOr(
		Schema.Struct({ key: Schema.String, label: Schema.String }),
	),
	found: Schema.NullOr(
		Schema.Struct({
			art: Schema.optionalKey(Artwork),
			meta: Schema.optionalKey(GameMeta),
		}),
	),
});
export type Verdict = typeof Verdict.Type;

/** A found game keeps 30 days; a miss is asked again after 7, so art added upstream shows up. */
export const HIT_TTL_MS = 30 * 24 * 3600_000;
export const MISS_TTL_MS = 7 * 24 * 3600_000;

export const META_FIELDS: ReadonlyArray<MetaField> = [
	"platform",
	"description",
	"developer",
	"publisher",
	"release_year",
	"genres",
	"tags",
	"region",
	"players",
];

const present = (v: unknown): boolean =>
	Array.isArray(v)
		? v.length > 0
		: typeof v === "string"
			? v.trim().length > 0
			: v !== null && v !== undefined;

/** The entry as its lister sent it: values a metadata source borrowed in are gone; a pick stays. */
export const ownView = (e: LibraryEntry): LibraryEntry => {
	const filled = e.filled ?? {};
	const art: Record<string, unknown> = { ...(e.art ?? {}) };
	for (const k of ART_KINDS) {
		if (filled[k] !== undefined && filled[k] !== "pick") delete art[k];
	}
	const out: Record<string, unknown> = { ...e, art };
	for (const f of META_FIELDS) if (filled[f] !== undefined) delete out[f];
	delete out.filled;
	return out as LibraryEntry;
};

/**
 * Worth a lookup. Never a launcher tile. Otherwise: an offered slot or field the entry lacks —
 * or, set to replace, any offered slot the operator has not picked.
 */
export const wanted = (
	e: LibraryEntry,
	offers: Offers,
	replace: boolean,
): boolean => {
	if (e.role === "launcher") return false;
	const own = ownView(e);
	const filled = e.filled ?? {};
	const art = (offers.art ?? []).some(
		(k) => filled[k] !== "pick" && (replace || !present(own.art?.[k])),
	);
	return art || (offers.meta ?? []).some((f) => !present(own[f]));
};

/** What a verdict is keyed on: the lister's title, platform and ids, plus the operator's pin. */
export const identityOf = (
	e: LibraryEntry,
	pin: string | undefined,
): string => {
	const own = ownView(e);
	const ids = Object.entries(own.ids ?? {}).sort(([a], [b]) =>
		a < b ? -1 : a > b ? 1 : 0,
	);
	return JSON.stringify([own.title, own.platform ?? null, ids, pin ?? null]);
};

export const isFresh = (
	v: Verdict | undefined,
	identity: string,
	matcherVersion: number,
	now: number,
): boolean =>
	v !== undefined &&
	v.v === matcherVersion &&
	v.identity === identity &&
	now - v.at < (v.found ? HIT_TTL_MS : MISS_TTL_MS);

/** An `http(s)` URL the host will store. */
export const isHttpUrl = (u: unknown): u is string =>
	typeof u === "string" &&
	/^https?:\/\//.test(u) &&
	u.length <= 2048 &&
	!/\s/.test(u);

/** What `found` puts in a push: offered slots and fields only, art only as `http(s)`. */
export const toRow = (
	id: string,
	found: Found,
	offers: Offers,
): MetadataEntry | undefined => {
	const art: Record<string, string> = {};
	for (const k of offers.art ?? []) {
		const u = found.art?.[k];
		if (isHttpUrl(u)) art[k] = u;
	}
	const meta: Record<string, unknown> = {};
	for (const f of offers.meta ?? []) {
		const v = found.meta?.[f];
		if (present(v)) meta[f] = v;
	}
	const hasArt = Object.keys(art).length > 0;
	const hasMeta = Object.keys(meta).length > 0;
	if (!hasArt && !hasMeta) return undefined;
	return {
		id,
		...(hasArt ? { art } : {}),
		...(hasMeta ? { meta } : {}),
	} as MetadataEntry;
};
