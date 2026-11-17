// The desktop's own mode and accent, read from the host (design/web-console-overhaul.md §7).
//
// Second in precedence behind Omarchy, which stays first because it carries the full four-value
// palette — background and foreground included — while this carries only the two values a
// desktop actually publishes. An Omarchy box answers both; taking the richer one is what makes
// the console look like it belongs to the desktop rather than merely agreeing with its accent.
//
// Read per request, like `omarchyTheme`. The console polls `ui-config` every two seconds, so an
// accent changed on the desktop reaches the page on the same tick.
import { mgmtFetch } from "./forward";

export interface HostTheme {
	/** `portal` | `windows`, or `null` when nothing answered. */
	source: string | null;
	mode: "light" | "dark" | null;
	/** `#rrggbb`, or `null`. */
	accent: string | null;
}

/** Whether anything was actually reported — an all-null answer is "none detected". */
export function hasTheme(t: HostTheme | null): t is HostTheme {
	return !!t && (t.mode !== null || t.accent !== null);
}

const HEX = /^#[0-9a-f]{6}$/i;

/**
 * `null` on any failure, which is every host older than this route and every desktop with no
 * portal. The console then falls back to its own palette rather than to a guess.
 *
 * The accent reaches a `style` attribute, so it is validated HERE rather than trusted because it
 * came from the host: the host reads it from a registry value or a D-Bus reply, neither of which
 * this process controls.
 */
export async function hostTheme(): Promise<HostTheme | null> {
	try {
		const res = await mgmtFetch("/api/v1/host/theme", {
			// A theme is decoration: it must never hold up the page it decorates.
			signal: AbortSignal.timeout(1500),
		});
		if (!res.ok) return null;
		const body = (await res.json()) as Partial<HostTheme>;
		const accent =
			typeof body.accent === "string" && HEX.test(body.accent)
				? body.accent
				: null;
		const mode =
			body.mode === "light" || body.mode === "dark" ? body.mode : null;
		return {
			source: typeof body.source === "string" ? body.source : null,
			mode,
			accent,
		};
	} catch {
		return null;
	}
}
