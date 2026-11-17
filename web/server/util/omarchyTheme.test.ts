// The accent from this file is inlined into a style attribute, so the colour validator is the one
// piece here that is security-relevant rather than merely cosmetic — and "no theme" has to be the
// answer for every kind of missing, half-written or hostile input, because the console's own
// palette is a perfectly good fallback and an error page is not.
import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { omarchyTheme } from "./omarchyTheme";

/** A rendered theme file, with `over` replacing any of its fields. `omarchyTheme` requires the
 *  three colours, so a test that names only the field it is about still gets a valid file. */
function rendered(over: Record<string, unknown> = {}): string {
	return JSON.stringify({
		mode: "dark",
		background: "#1e1e2e",
		foreground: "#cdd6f4",
		accent: "#89b4fa",
		...over,
	});
}

/** Point `omarchyTheme` at a scratch XDG_STATE_HOME holding `content` (or nothing). */
function withTheme<T>(content: string | null, fn: () => T): T {
	const dir = mkdtempSync(join(tmpdir(), "pf-theme-"));
	if (content !== null) {
		const d = join(dir, "omarchy", "current", "theme");
		mkdirSync(d, { recursive: true });
		writeFileSync(join(d, "punktfunk.json"), content);
	}
	const prev = process.env.XDG_STATE_HOME;
	process.env.XDG_STATE_HOME = dir;
	try {
		return fn();
	} finally {
		if (prev === undefined) delete process.env.XDG_STATE_HOME;
		else process.env.XDG_STATE_HOME = prev;
	}
}

describe("omarchyTheme", () => {
	test("reads the whole palette from a rendered template", () => {
		expect(withTheme(rendered(), omarchyTheme)).toEqual({
			mode: "dark",
			background: "#1e1e2e",
			foreground: "#cdd6f4",
			accent: "#89b4fa",
		});
	});

	test("light mode survives; anything else is dark", () => {
		expect(withTheme(rendered({ mode: "light" }), omarchyTheme)?.mode).toBe(
			"light",
		);
		expect(withTheme(rendered({ mode: "nonsense" }), omarchyTheme)?.mode).toBe(
			"dark",
		);
	});

	test("a partial palette is no theme, not a half-themed console", () => {
		// An accent alone leaves the violet chrome under a themed button, and every surface is
		// mixed from the background/foreground pair, so a file carrying neither has nothing to
		// mix — fall back to the console's own palette entire.
		expect(
			withTheme('{"mode":"dark","accent":"#89b4fa"}', omarchyTheme),
		).toBeNull();
		expect(
			withTheme(rendered({ background: undefined }), omarchyTheme),
		).toBeNull();
		expect(
			withTheme(rendered({ foreground: undefined }), omarchyTheme),
		).toBeNull();
	});

	test("no file is no theme, not an error", () => {
		expect(withTheme(null, omarchyTheme)).toBeNull();
	});

	test("an UNRENDERED template is refused", () => {
		// The exact shape of a `.tpl` Omarchy never rendered — the placeholder is not a colour, and
		// letting it through would put `{{ accent }}` into a style declaration.
		expect(
			withTheme(
				'{"mode":"{{ mode }}","background":"{{ background }}",' +
					'"foreground":"{{ foreground }}","accent":"{{ accent }}"}',
				omarchyTheme,
			),
		).toBeNull();
	});

	test("refuses anything that could break out of a style declaration", () => {
		const hostile = [
			"red; background: url(http://evil/)",
			"#fff; --primary: blue",
			"</style><script>alert(1)</script>",
			"expression(alert(1))",
			"url(javascript:alert(1))",
			"#".repeat(200),
		];
		// EVERY colour reaches the DOM, not just the accent — background and foreground are inlined
		// into the same style attribute, so each of the three has to be validated.
		for (const field of ["accent", "background", "foreground"]) {
			for (const value of hostile) {
				expect(
					withTheme(rendered({ [field]: value }), omarchyTheme),
				).toBeNull();
			}
		}
	});

	test("accepts the notations Omarchy themes actually use", () => {
		for (const accent of [
			"#89b4fa",
			"#fff",
			"#89b4faff",
			"rgb(137, 180, 250)",
			"oklch(0.7 0.1 250)",
		]) {
			expect(withTheme(rendered({ accent }), omarchyTheme)?.accent).toBe(
				accent,
			);
		}
	});

	test("a half-written file during a theme switch is no theme", () => {
		expect(withTheme('{"mode":"dark","acc', omarchyTheme)).toBeNull();
	});
});
