// The i18n reviewer's checklist as much as a regression net: if a clause reads wrong here, it
// reads wrong on the page, because the page renders exactly this (design §5.2).
import { describe, expect, test } from "bun:test";
import type { EffectivePolicy } from "@/api/gen/model";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { describePolicy } from "./describePolicy";

const base: EffectivePolicy = {
	keep_alive: { mode: "duration", seconds: 10 },
	topology: "extend",
	mode_conflict: "separate",
	identity: "per-client",
	layout: { mode: "auto-row", positions: {} },
	max_displays: 4,
};

const policy = (over: Partial<EffectivePolicy>): EffectivePolicy => ({
	...base,
	...over,
});

/** The five shipped presets, by the fields the host expands them to. */
const PRESETS: Record<string, EffectivePolicy> = {
	default: base,
	"gaming-rig": policy({
		keep_alive: { mode: "forever" },
		topology: "primary",
		mode_conflict: "steal",
	}),
	"shared-desktop": policy({ keep_alive: { mode: "off" }, topology: "extend" }),
	hotdesk: policy({ topology: "primary", mode_conflict: "reject" }),
	workstation: policy({ topology: "exclusive", mode_conflict: "join" }),
};

describe("describePolicy (en)", () => {
	test.each([
		[
			"default",
			"Each device gets its own screen next to your monitors, kept 10 s after it disconnects. A second device gets its own screen.",
		],
		[
			"gaming-rig",
			"Each device gets the main screen, kept until you release it. A second device takes over.",
		],
		[
			"shared-desktop",
			"Each device gets its own screen next to your monitors, removed when it disconnects. A second device gets its own screen.",
		],
		[
			"hotdesk",
			"Each device gets the main screen, kept 10 s after it disconnects. A second device is told the host is busy.",
		],
		[
			"workstation",
			"Each device gets the only screen — your monitors turn off, kept 10 s after it disconnects. A second device shares the screen.",
		],
	])("%s", (id, expected) => {
		expect(describePolicy(PRESETS[id])).toBe(expected);
	});

	// `auto` is the host deciding per setup. Naming an outcome would be a guess it has not made.
	test("auto topology commits to nothing", () => {
		expect(describePolicy(policy({ topology: "auto" }))).toContain(
			"a screen chosen to fit this host",
		);
	});

	// The tail is the whole point of deleting the old pending note: it was shown always, so it
	// said nothing. It is true only while a session is holding the old policy.
	test("the next-connect tail appears only while a session is live", () => {
		expect(describePolicy(base)).not.toContain(
			"Applies to the next connection.",
		);
		expect(describePolicy(base, { live: true })).toEndWith(
			"Applies to the next connection.",
		);
	});

	test("a mirror pin is the screen, and dedicated game sessions are said", () => {
		expect(describePolicy(base, { mirror: "DP-1" })).toStartWith(
			"Each device sees DP-1.",
		);
		expect(describePolicy(base, { gameSession: "dedicated" })).toContain(
			"Games open in their own session.",
		);
		expect(describePolicy(base, { gameSession: "auto" })).not.toContain(
			"Games",
		);
	});

	test("a named device speaks about that device", () => {
		expect(describePolicy(base, { deviceName: "Living-room TV" })).toStartWith(
			"Living-room TV gets",
		);
	});
});

describe("describePolicy (de)", () => {
	// `setLocale` cannot move the locale here: the configured strategies are localStorage and
	// preferredLanguage, both guarded on a browser, so off one every call resolves to the base
	// locale. `overwriteGetLocale` is paraglide's own hook for exactly this.
	test("every clause is translated, not passed through", () => {
		overwriteGetLocale(() => "de");
		try {
			const sentence = describePolicy(PRESETS["gaming-rig"], { live: true });
			expect(sentence).toBe(
				"Jedes Gerät bekommt den Hauptbildschirm, der bleibt, bis du ihn freigibst. Ein zweites Gerät übernimmt. Gilt ab der nächsten Verbindung.",
			);
			// A missed clause shows up as English inside a German sentence.
			expect(sentence).not.toContain("device");
			expect(sentence).not.toContain("screen");
		} finally {
			overwriteGetLocale(() => baseLocale);
		}
	});
});
