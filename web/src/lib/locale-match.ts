// Which shipped language a browser's preference list asks for. The same walk as the Rust `pick`
// in l10n/src/lib.rs, whose cases (l10n/vectors.json) the tests replay; `zh` is the one addition.

/** Regions that write Chinese in the traditional script. Every other `zh` is simplified. */
const TRADITIONAL = new Set(["TW", "HK", "MO"]);

/** `zh-TW` is `zh-Hant-TW` and `zh-CN` is `zh-Hans-CN`. A tag that names its script stays. */
function withChineseScript(tag: string): string {
	const [lang = "", ...tail] = tag.split("-");
	const next = tail[0];
	if (lang.toLowerCase() !== "zh" || (next && /^[a-z]{4}$/i.test(next))) {
		return tag;
	}
	const script = TRADITIONAL.has(next?.toUpperCase() ?? "") ? "Hant" : "Hans";
	return [lang, script, ...tail].join("-");
}

/**
 * The shipped locale that best matches `tags`, most preferred first, or `undefined`. Each tag is
 * tried as it is, then through `fallback`, then one subtag shorter (`de-AT`, `de`). Case and `_`
 * do not matter.
 */
export function matchLocale<L extends string>(
	tags: readonly string[],
	shipped: readonly L[],
	fallback: Readonly<Record<string, string>> = {},
): L | undefined {
	const find = (tag: string | undefined) =>
		shipped.find((l) => l.toLowerCase() === tag?.toLowerCase());
	for (const tag of tags) {
		let t = withChineseScript(tag.trim().replace(/_/g, "-"));
		for (;;) {
			const via = Object.entries(fallback).find(
				([from]) => from.toLowerCase() === t.toLowerCase(),
			);
			const hit = find(t) ?? find(via?.[1]);
			if (hit) return hit;
			const cut = t.lastIndexOf("-");
			if (cut < 0) break;
			t = t.slice(0, cut);
		}
	}
	return undefined;
}
