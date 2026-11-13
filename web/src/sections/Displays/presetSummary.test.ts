// The built-in presets say what they do in the console's words, not the host's line.
import { expect, test } from "bun:test";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { presetSummary } from "./Behaviour";

overwriteGetLocale(() => baseLocale);

const IDS = [
	"default",
	"gaming-rig",
	"shared-desktop",
	"hotdesk",
	"workstation",
	"play-together",
];

test("each built-in preset replaces the host's summary", () => {
	for (const id of IDS) {
		const summary = presetSummary(id, "The host's line.");
		expect(summary).not.toBe("The host's line.");
		expect(summary).not.toContain("display_preset");
	}
	expect(presetSummary("hotdesk", "x")).toStartWith("One person at a time");
});

test("a preset this build does not know keeps the host's line", () => {
	expect(presetSummary("from-a-newer-host", "The host's line.")).toBe(
		"The host's line.",
	);
});
