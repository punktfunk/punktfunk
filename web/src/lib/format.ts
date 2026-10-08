import { getLocale } from "@/paraglide/runtime";

/**
 * Date/time and number formatting that follows the CONSOLE's locale, not the browser's.
 *
 * A bare `toLocaleString()` uses `navigator.language`, so a console switched to German still
 * rendered US-style timestamps (and vice versa) — the app said one thing and its dates another.
 * `getLocale()` is Paraglide's resolved locale, which is what every string on screen uses.
 *
 * `Intl` formatters are expensive to construct and these run per table row, so they are cached
 * per locale.
 */
const dateTimeCache = new Map<string, Intl.DateTimeFormat>();

function dateTimeFor(locale: string): Intl.DateTimeFormat {
	let f = dateTimeCache.get(locale);
	if (!f) {
		f = new Intl.DateTimeFormat(locale, {
			dateStyle: "medium",
			timeStyle: "short",
		});
		dateTimeCache.set(locale, f);
	}
	return f;
}

/** Unix MILLISECONDS → a locale date-time, or an em dash for "never". */
export function fmtDateTime(unixMs: number | undefined | null): string {
	if (!unixMs) return "—";
	return dateTimeFor(getLocale()).format(new Date(unixMs));
}

/** Unix SECONDS → a locale date-time (the store's `fetched_at` convention). */
export function fmtDateTimeSecs(unixSecs: number | undefined | null): string {
	if (!unixSecs) return "—";
	return fmtDateTime(unixSecs * 1000);
}

/**
 * Seconds → `m:ss`, a duration and not a number of seconds. With `hours`, `h:mm` from an hour
 * on, so a session's age stays short.
 */
export function fmtClockDuration(
	secs: number,
	{ hours = false }: { hours?: boolean } = {},
): string {
	const s = Math.max(0, Math.floor(secs));
	const pad2 = (n: number) => String(n).padStart(2, "0");
	if (hours && s >= 3600)
		return `${Math.floor(s / 3600)}:${pad2(Math.floor((s % 3600) / 60))}`;
	return `${Math.floor(s / 60)}:${pad2(s % 60)}`;
}

const AGO_STEPS: [Intl.RelativeTimeFormatUnit, number][] = [
	["day", 86_400],
	["hour", 3_600],
	["minute", 60],
];

/** Unix SECONDS → "2 hours ago", in the console's locale; under a minute is "now". */
export function fmtAgo(unixSecs: number, nowSecs = Date.now() / 1000): string {
	const rtf = new Intl.RelativeTimeFormat(getLocale(), { numeric: "auto" });
	const secs = Math.max(0, nowSecs - unixSecs);
	for (const [unit, size] of AGO_STEPS)
		if (secs >= size) return rtf.format(-Math.floor(secs / size), unit);
	return rtf.format(0, "second");
}

/** Seconds → "42 min", "1 hr 12 min": a span someone reads at a glance, minute-accurate. */
export function fmtSpan(secs: number): string {
	const unit = (unit: string, n: number) =>
		new Intl.NumberFormat(getLocale(), {
			style: "unit",
			unit,
			unitDisplay: "short",
		}).format(n);
	const mins = Math.max(0, Math.floor(secs / 60));
	if (mins < 60) return unit("minute", mins);
	const rest = mins % 60;
	const hours = unit("hour", Math.floor(mins / 60));
	return rest ? `${hours} ${unit("minute", rest)}` : hours;
}

/** A number with the console locale's separators — never a hand-rolled `toFixed`. */
/** A link rate in kbps as a player reads it: `940 Mbit/s`, `2.5 Gbit/s`. */
export function fmtLinkRate(kbps: number): string {
	if (kbps >= 1_000_000) {
		return `${fmtNumber(kbps / 1_000_000, kbps % 1_000_000 === 0 ? 0 : 1)} Gbit/s`;
	}
	return `${fmtNumber(kbps / 1_000)} Mbit/s`;
}

export function fmtNumber(value: number, digits = 0): string {
	return new Intl.NumberFormat(getLocale(), {
		minimumFractionDigits: digits,
		maximumFractionDigits: digits,
	}).format(value);
}
