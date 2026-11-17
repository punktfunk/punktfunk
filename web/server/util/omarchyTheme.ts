// The Omarchy theme, if this box has one.
//
// Omarchy renders every registered `~/.config/omarchy/themed/*.tpl` from the active theme's
// semantic `colors.toml` on each `omarchy-theme-set`, dropping the result in
// `~/.local/state/omarchy/current/theme/`. We register `punktfunk.json.tpl` (installed by
// `punktfunk-omarchy setup`, opt-in), so the file below is the theme expressed in exactly the four
// values the console can act on.
//
// Deliberately a FILE read and not an integration: there is no Omarchy API to call, the host learns
// nothing, and a box that never opted in simply has no file — which is why every failure here is
// "no theme", never an error. The console's own palette is the fallback.
//
// All four values are carried, not just the accent: an accent alone leaves the console's own violet
// chrome under a themed button, which is what "the theme is not fully applied" means in practice.
// `styles.css` mixes the surfaces out of the background/foreground pair.
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export interface OmarchyTheme {
	/** `light` | `dark` — drives the `.dark` class the whole palette keys off. */
	mode: "light" | "dark";
	/** The theme's accent, as a CSS colour. Mapped onto `--pf-brand`, which `--primary`,
	 *  `--accent` and `--ring` all derive from, so one value re-tints the console.
	 *  Optional: a desktop may publish a mode and no accent. */
	accent?: string;
	/** The desktop's own page colour. Mapped onto `--pf-bg`, which the console's cards, hovers
	 *  and borders are all mixed out of — this is the value that makes the console look like it
	 *  belongs to the theme rather than merely agreeing with its accent.
	 *
	 *  Optional since the theme can also come from the HOST's read of the desktop
	 *  (design/web-console-overhaul.md §7), which publishes mode and accent only. Absent leaves
	 *  the console's own surfaces in place — half a palette is worse than none. */
	background?: string;
	/** The desktop's own text colour (`--pf-fg`), and the other end of every one of those mixes. */
	foreground?: string;
	/** Which reader answered: `omarchy`, or the host's `portal` / `windows`. Names the source
	 *  on the Appearance panel's "Follow host" position. */
	source?: string;
}

/** Where Omarchy renders our template. `XDG_STATE_HOME` first, because that is what the spec says
 * and what a non-default setup uses; `~/.local/state` is the default it falls back to. */
function themePath(): string {
	const state =
		process.env.XDG_STATE_HOME?.trim() || join(homedir(), ".local", "state");
	return join(state, "omarchy", "current", "theme", "punktfunk.json");
}

/** A CSS colour we are willing to inline into a style attribute.
 *
 * This is the security-relevant line in the file: the value reaches the DOM, so anything that could
 * close the declaration and start another one is refused. Hex and the common functional notations
 * cover every theme Omarchy ships; anything else falls back to the console's own brand rather than
 * being sanitised into something half-right. */
function isSafeColor(v: unknown): v is string {
	return (
		typeof v === "string" &&
		v.length <= 64 &&
		/^(#[0-9a-fA-F]{3,8}|(rgb|rgba|hsl|hsla|oklch|oklab)\([0-9a-zA-Z.,%/\s-]+\))$/.test(
			v.trim(),
		)
	);
}

/**
 * The active Omarchy theme, or `null` when this box has none — which is every box that is not
 * Omarchy, and every Omarchy box whose operator did not opt in.
 *
 * Read per request rather than cached: `omarchy-theme-set` rewrites the file whenever the user
 * changes theme, and a console that only noticed at startup would be wrong until it restarted.
 * It is one small local read behind an already-authenticated route.
 */
export function omarchyTheme(): OmarchyTheme | null {
	let raw: string;
	try {
		raw = readFileSync(themePath(), "utf8");
	} catch {
		return null; // no file: not Omarchy, or not opted in
	}
	try {
		const parsed = JSON.parse(raw) as Record<string, unknown>;
		const mode = parsed.mode === "light" ? "light" : "dark";
		// An unrendered template still contains its `{{ accent }}` placeholder — that is not a
		// colour, and `isSafeColor` is what stops it reaching the page as one. All THREE colours
		// are required: the template renders them together, so a file missing one is a file we do
		// not understand, and half a palette reads worse than the console's own.
		if (
			!isSafeColor(parsed.accent) ||
			!isSafeColor(parsed.background) ||
			!isSafeColor(parsed.foreground)
		)
			return null;
		return {
			mode,
			accent: parsed.accent.trim(),
			background: parsed.background.trim(),
			foreground: parsed.foreground.trim(),
		};
	} catch {
		return null; // half-written during a theme switch, or hand-edited into invalid JSON
	}
}
