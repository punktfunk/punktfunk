// A host check is worded by its field ids, with the host's English for any id this build lacks.
import { afterEach, expect, test } from "bun:test";
import type { HostCheck } from "@/api/gen/model/hostCheck";
import { baseLocale, overwriteGetLocale } from "@/paraglide/runtime";
import { checkImpact, checkSummary, remedyText } from "./diagnostics";

overwriteGetLocale(() => baseLocale);
afterEach(() => overwriteGetLocale(() => baseLocale));

const check = (over: Partial<HostCheck>): HostCheck =>
	({
		id: "pad_audio",
		status: "warn",
		severity: "warning",
		source: "startup",
		summary: "English summary.",
		impact: "English impact.",
		params: {},
		...over,
	}) as HostCheck;

test("summary, impact and remedy are worded by their own ids", () => {
	const c = check({
		summary_id: "diag_pad_audio_ok_summary",
		impact_id: "diag_pad_audio_refused_impact",
		remedy_id: "diag_pad_audio_refused_remedy",
		remedy: { text: "English remedy." },
		params: { count: "2" },
	});
	expect(checkSummary(c)).toBe(
		"The controller speaker endpoint takes the host's open (2 pad slots).",
	);
	overwriteGetLocale(() => "de");
	expect(checkSummary(c)).toBe(
		"Der Lautsprecher-Endpunkt für Controller nimmt die Verbindung des Hosts an (2 Controller-Plätze).",
	);
	expect(checkImpact(c)).toStartWith("Spiele können den DualSense nicht");
	expect(remedyText(c)).toStartWith("Aktualisiere Steam");
});

test("each field falls back to the host's English on its own", () => {
	overwriteGetLocale(() => "de");
	const c = check({
		summary_id: "diag_pad_audio_ok_summary",
		impact_id: "diag_from_a_newer_host_impact",
		remedy: { text: "English remedy." },
		params: { count: "1" },
	});
	expect(checkSummary(c)).toContain("1 Controller-Platz");
	expect(checkImpact(c)).toBe("English impact.");
	expect(remedyText(c)).toBe("English remedy.");
	expect(remedyText(check({}))).toBeUndefined();
});

test("a check without ids keeps the English it arrived with", () => {
	const c = check({});
	expect(checkSummary(c)).toBe("English summary.");
	expect(checkImpact(c)).toBe("English impact.");
});
