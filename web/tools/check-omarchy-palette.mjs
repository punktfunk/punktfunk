// Guards the Omarchy palette derivation in src/styles.css.
//
// That block turns three values from the desktop — background, foreground, accent — into every
// surface the console paints, using `color-mix(in oklab, …)`. The ratios in it are not taste: each
// one was picked by measuring contrast across real Omarchy themes, and nudging one by ten points
// is enough to put grey text on a grey card for somebody whose theme we have never seen. Nothing
// else in the repo can catch that — the mixes are resolved by the browser, so the typecheck, the
// unit tests and the build all pass on a palette that is unreadable.
//
// So: read the ratios back OUT of the stylesheet, redo the mixes here, and assert WCAG contrast
// against a table of shipped themes. It fails on a re-tune that breaks readability, and stays
// quiet on one that does not.
//
// Run by `postbuild`, beside check-i18n. No dependencies — oklab is about forty lines of maths.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const CSS = join(
	dirname(fileURLToPath(import.meta.url)),
	"..",
	"src",
	"styles.css",
);

/** Real themes Omarchy ships, as `[background, foreground, accent]`. Everforest Light is in here
 *  on purpose: its own foreground is only 5.2:1 against its own background, so it is the floor
 *  that decides what a *derived* muted colour can possibly reach. */
const THEMES = {
	"Tokyo Night (dark)": ["#1a1b26", "#a9b1d6", "#7aa2f7"],
	"Gruvbox (dark)": ["#282828", "#ebdbb2", "#d79921"],
	"Nord (dark)": ["#2e3440", "#d8dee9", "#88c0d0"],
	"Catppuccin Latte (light)": ["#eff1f5", "#4c4f69", "#1e66f5"],
	"Rose Pine Dawn (light)": ["#faf4ed", "#575279", "#d7827e"],
	"Everforest Light": ["#fdf6e3", "#5c6a72", "#8da101"],
};

/** Accents that reach the console WITHOUT a background/foreground pair
 *  (design/web-console-overhaul.md §7.3): a desktop over the XDG portal or DWM, and the
 *  operator's own pick. Only the accent-derived tokens apply, so each is checked against the
 *  console's OWN two surfaces rather than a theme's.
 *
 *  Every desktop default here is a real one — a tune that leaves any of them unreadable on
 *  either console background should fail the build, not the operator's eyes. */
const ACCENTS = {
	"Windows default": "#0078d4",
	"libadwaita blue": "#3584e4",
	"libadwaita teal": "#2190a4",
	"libadwaita green": "#3a944a",
	"libadwaita yellow": "#c88800",
	"libadwaita orange": "#ed5b00",
	"libadwaita red": "#e62d42",
	"libadwaita pink": "#d56199",
	"libadwaita purple": "#9141ac",
	"libadwaita slate": "#6f8396",
	"Breeze blue": "#3daee9",
	// The console's own swatch row (src/lib/appearance.ts).
	"punktfunk violet": "#6c5bf3",
	"swatch blue": "#3584e4",
	"swatch green": "#2ec27e",
	"swatch amber": "#c88800",
	"swatch orange": "#e66100",
	"swatch red": "#c01c28",
	"swatch purple": "#c061cb",
	"swatch slate": "#6f8396",
};

/** The page and text colours the accent-only checks run on. They are literals, not read from
 *  styles.css: its dark `--pf-page` and light `--foreground` are other values. */
const CONSOLE_SURFACES = {
	light: { bg: "#ffffff", fg: "#0a0a0a" },
	dark: { bg: "#0a0a0a", fg: "#fafafa" },
};

// ── colour maths: sRGB ⇄ Oklab (Ottosson), and WCAG 2.1 relative luminance ──────────────────
const hex = (h) => {
	const s = h.replace("#", "");
	return [0, 2, 4].map((i) => Number.parseInt(s.slice(i, i + 2), 16) / 255);
};
const toLin = (c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
const fromLin = (c) =>
	c <= 0.0031308 ? 12.92 * c : 1.055 * c ** (1 / 2.4) - 0.055;
const cbrt = (v) => (v >= 0 ? Math.cbrt(v) : -Math.cbrt(-v));

function toOklab([r0, g0, b0]) {
	const [r, g, b] = [toLin(r0), toLin(g0), toLin(b0)];
	const l = cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
	const m = cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
	const s = cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
	return [
		0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
		1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
		0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
	];
}

function fromOklab([L, A, B]) {
	const l = (L + 0.3963377774 * A + 0.2158037573 * B) ** 3;
	const m = (L - 0.1055613458 * A - 0.0638541728 * B) ** 3;
	const s = (L - 0.0894841775 * A - 1.291485548 * B) ** 3;
	return [
		4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
		-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
		-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
	].map((c) => Math.min(1, Math.max(0, fromLin(c))));
}

/** `color-mix(in oklab, a pct%, b)`. */
const mix = (a, pct, b) => {
	const [x, y] = [toOklab(a), toOklab(b)];
	return fromOklab(x.map((v, i) => (pct / 100) * v + (1 - pct / 100) * y[i]));
};

const lum = ([r, g, b]) =>
	0.2126 * toLin(r) + 0.7152 * toLin(g) + 0.0722 * toLin(b);
const contrast = (a, b) => {
	const [hi, lo] = [lum(a), lum(b)].sort((p, q) => q - p);
	return (hi + 0.05) / (lo + 0.05);
};

// ── read the ratios back out of the stylesheet ─────────────────────────────────────────────
const css = readFileSync(CSS, "utf8");

/** The body of one rule, so a ratio is read from the block that owns it. Several
 *  blocks declare `--card`; an unscoped search takes whichever comes first in the file and checks
 *  the wrong derivation without saying so. */
function rule(selector) {
	const open = css.indexOf(`${selector} {`);
	if (open < 0) {
		throw new Error(
			`check-omarchy-palette: no \`${selector}\` rule in styles.css. If the derivation ` +
				"was restructured, update this check with it.",
		);
	}
	return css.slice(open, css.indexOf("}", open));
}

/** The percentage in `--<token>: color-mix(in oklab, <first> N%, <second>)`, within one rule.
 *  Throws rather than defaulting: a declaration this cannot find is one nobody is checking. */
function ratio(token, selector) {
	const scope = rule(selector);
	// Bounded by the declaration's own `;` rather than by the end of the line: Biome wraps a
	// long color-mix across five lines, and a line-bound match then reads nothing while a
	// newline-crossing one would happily take the NEXT declaration's percentage.
	const decl = scope.match(new RegExp(`--${token}:([\\s\\S]*?);`));
	const m = decl?.[1].match(/color-mix\(\s*in oklab,[\s\S]*?(\d+)%/);
	if (!m) {
		throw new Error(
			`check-omarchy-palette: no --${token} color-mix in \`${selector}\`. If the ` +
				"derivation was restructured, update this check with it.",
		);
	}
	return Number(m[1]);
}

const OMARCHY = ':root[data-omarchy]:not([data-accent="custom"])';
const ACCENT = ":root[data-accent]";

const R = {
	card: ratio("card", OMARCHY),
	muted: ratio("muted", OMARCHY),
	mutedFg: ratio("muted-foreground", OMARCHY),
	secondary: ratio("secondary", OMARCHY),
	border: ratio("border", OMARCHY),
	accent: ratio("accent", OMARCHY),
	lift: ratio("pf-lift", OMARCHY),
	brand: ratio("pf-brand", ACCENT),
	brandLight: ratio("pf-brand-light", ACCENT),
	highlight: ratio("pf-highlight", ACCENT),
	/* The accent-only half tints the console's own surfaces too. */
	aBackground: ratio("background", ACCENT),
	aCard: ratio("card", ACCENT),
	aMuted: ratio("muted", ACCENT),
	aSecondary: ratio("secondary", ACCENT),
	aBorder: ratio("border", ACCENT),
	// The light-mode override, in its own `:not(.dark)` rule.
	primaryLight: Number(
		css.match(
			/:not\(\.dark\)\s*\{[\s\S]*?--primary:\s*color-mix\(in oklab,.*?(\d+)%/,
		)?.[1] ?? Number.NaN,
	),
};
if (Number.isNaN(R.primaryLight)) {
	throw new Error(
		"check-omarchy-palette: no light-mode --primary override found in styles.css.",
	);
}

const WHITE = [1, 1, 1];
const BLACK = [0, 0, 0];
const failures = [];

for (const [name, [bgH, fgH, acH]] of Object.entries(THEMES)) {
	const dark = lum(hex(bgH)) < lum(hex(fgH));
	const [bg, fg, ac] = [hex(bgH), hex(fgH), hex(acH)];

	// The surface lift carries a quarter of the accent; --muted-foreground is text and
	// stays the theme's own foreground.
	const lift = mix(fg, R.lift, ac);
	const card = mix(bg, R.card, lift);
	const mutedFg = mix(fg, R.mutedFg, bg);
	const border = mix(bg, R.border, lift);
	const accent = mix(bg, R.accent, ac);
	const brand = mix(ac, R.brand, BLACK);
	// Toward the page's own far end, which is what --pf-tint-end carries in the stylesheet.
	const end = dark ? WHITE : BLACK;
	const brandLight = mix(ac, R.brandLight, end);
	const highlight = mix(ac, R.highlight, end);
	// `.dark` puts --primary on the light tint with the theme's background as its text;
	// light mode uses its own deepened override with :root's white.
	const primary = dark ? brandLight : mix(ac, R.primaryLight, BLACK);
	const primaryFg = dark ? bg : WHITE;

	// Each floor is what the derivation was measured to hold, NOT an aspiration. Where a floor
	// sits below WCAG AA the reason is the theme's own headroom, and it is named.
	const checks = [
		["foreground on card", fg, card, 4.5],
		// A theme whose own foreground is ~5:1 on its own background cannot yield a MUTED
		// variant that clears 4.5 on a card. 3.7 is the measured floor across this table.
		["muted-foreground on card", mutedFg, card, 3.7],
		["text on a primary button", primaryFg, primary, 4.5],
		["foreground on the accent surface", fg, accent, 4.5],
		["card distinguishable from background", card, bg, 1.05],
		["border distinguishable from card", border, card, 1.1],
		// The lens mark is three tints of one accent; too close and it reads as a blob.
		["mark: light circle vs deep circle", brandLight, brand, 1.4],
		["mark: highlight vs light circle", highlight, brandLight, 1.25],
		// ...and separation from each other is not visibility: three tints can sit well apart
		// and all near the page colour. --pf-highlight is the wordmark, so it reads as text.
		["mark: wordmark tone on the page", highlight, bg, 3],
		["mark: light circle on the page", brandLight, bg, 1.5],
		["mark: deep circle on the page", brand, bg, 1.5],
	];
	for (const [what, a, b, floor] of checks) {
		const r = contrast(a, b);
		if (r < floor) {
			failures.push(`${name}: ${what} is ${r.toFixed(2)}:1, floor ${floor}:1`);
		}
	}
}

// ── accent-only sources: the [data-accent] half of the split ────────────────────────────────
for (const [name, acH] of Object.entries(ACCENTS)) {
	const ac = hex(acH);
	// Mirrors :root[data-accent] in styles.css.
	const brand = mix(ac, R.brand, BLACK);
	for (const [mode, { bg: bgH, fg: fgH }] of Object.entries(CONSOLE_SURFACES)) {
		const [page, fg] = [hex(bgH), hex(fgH)];
		const dark = mode === "dark";
		const end = dark ? WHITE : BLACK;
		const brandLight = mix(ac, R.brandLight, end);
		const highlight = mix(ac, R.highlight, end);
		// The console's own surfaces, tinted from the page toward the accent.
		const bg = mix(page, R.aBackground, ac);
		const card = mix(page, R.aCard, ac);
		const muted = mix(page, R.aMuted, ac);
		const secondary = mix(page, R.aSecondary, ac);
		const border = mix(page, R.aBorder, ac);
		// Dark takes --primary from the light tint with the page background as its text;
		// light mixes the accent with black, as `:root[data-accent]:not(.dark)` does, with white
		// text. Same rule as the themed half.
		const primary = dark ? brandLight : mix(ac, 75, BLACK);
		const primaryFg = dark ? bg : WHITE;
		const checks = [
			["text on a primary button", primaryFg, primary, 4.5],
			["mark: light circle vs deep circle", brandLight, brand, 1.4],
			["mark: highlight vs light circle", highlight, brandLight, 1.25],
			["mark: wordmark tone on the page", highlight, bg, 3],
			["mark: light circle on the page", brandLight, bg, 1.5],
			["mark: deep circle on the page", brand, bg, 1.5],
			["primary distinguishable from the page", primary, bg, 1.5],
			// The page's own text over surfaces that now carry the accent.
			["foreground on background", fg, bg, 4.5],
			["foreground on card", fg, card, 4.5],
			["foreground on the secondary surface", fg, secondary, 4.5],
			["foreground on the muted surface", fg, muted, 4.5],
			["card distinguishable from background", card, bg, 1.02],
			["border distinguishable from card", border, card, 1.05],
		];
		for (const [what, a, b, floor] of checks) {
			const r = contrast(a, b);
			if (r < floor) {
				failures.push(
					`${name} (${mode}): ${what} is ${r.toFixed(2)}:1, floor ${floor}:1`,
				);
			}
		}
	}
}

if (failures.length > 0) {
	console.error(
		`✖ Omarchy palette: ${failures.length} contrast floor(s) breached by the ratios in ` +
			"src/styles.css:\n  " +
			failures.join("\n  "),
	);
	process.exit(1);
}
console.log(
	`✔ Omarchy palette: ${Object.keys(THEMES).length} themes and ` +
		`${Object.keys(ACCENTS).length} accents clear every contrast floor`,
);
