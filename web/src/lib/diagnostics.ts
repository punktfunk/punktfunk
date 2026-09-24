import type { HostCheck } from "@/api/gen/model/hostCheck";
import { m } from "@/paraglide/messages";

/**
 * Presentation rules for the host's diagnostics checks.
 *
 * The console and the host ship as **separate packages**, and canary setups pair console N with
 * host N±1 — so a check whose `id` this build has never heard of is a normal state, not an error.
 * Every host check therefore arrives with English `summary`/`impact`/`remedy.text` already filled
 * in, and this module's job is to *decorate* that: a localized name for the checks we know, a
 * readable heading for the ones we don't, and the badge vocabulary.
 *
 * What is deliberately NOT localized here is the situational prose. A single check has many shapes
 * (the vhci one alone has four distinct causes, each with its own remedy), and copying ~20 sentences
 * into a package that versions independently of the one that generates them is exactly the drift
 * this design set out to avoid. The host is the single source of that text; when it starts sending
 * a shape discriminator alongside `id`, the console can key localized prose off it without guessing.
 */

/** Localized names for the check ids this build knows about. */
const TITLES: Record<string, () => string> = {
	takeover_privilege: () => m.diag_takeover_privilege_title(),
	virtual_deck_vhci: () => m.diag_virtual_deck_vhci_title(),
	uinput_access: () => m.diag_uinput_access_title(),
	server_conflict: () => m.diag_server_conflict_title(),
	vdisplay_driver: () => m.diag_vdisplay_driver_title(),
	pad_audio: () => m.diag_pad_audio_title(),
	pad_driver: () => m.diag_pad_driver_title(),
	restart_pending: () => m.diag_restart_pending_title(),
};

/**
 * A heading for a check. Unknown ids are turned into a presentable phrase rather than hidden — the
 * host's own text underneath still explains the problem, so showing it beats dropping it.
 */
export function checkTitle(check: HostCheck): string {
	const known = TITLES[check.id];
	if (known) return known();
	return check.id
		.replace(/_/g, " ")
		.replace(/^./, (first) => first.toUpperCase());
}

/** True when this build recognizes the check — used only to decide how much chrome to show. */
export function isKnownCheck(check: HostCheck): boolean {
	return check.id in TITLES;
}

/** Checks the operator should act on. `inapplicable` is not a problem; that is its whole point. */
export function needsAttention(check: HostCheck): boolean {
	return check.status === "warn" || check.status === "fail";
}

/**
 * The badge's text.
 *
 * For a row that needs attention this is the **severity**, not the status — because severity is
 * what the badge's colour encodes, and a badge whose text says "Failing" on both a red and an amber
 * row leaves the difference between them carried by colour alone. Anyone who cannot separate the
 * two tints then sees two identical rows. Status and severity only ever disagree in ways the reader
 * does not need ("degraded but critical"), so the badge says the thing that changes what they do.
 */
export function statusLabel(check: HostCheck): string {
	if (check.status === "ok") return m.diag_status_ok();
	if (check.status === "inapplicable") return m.diag_status_inapplicable();
	switch (check.severity) {
		case "critical":
			return m.diag_severity_critical();
		case "warning":
			return m.diag_severity_warning();
		default:
			return m.diag_severity_info();
	}
}

/**
 * Badge variant for a check. Text always says the state too — colour alone is not a state, the
 * same rule the pairing badge follows.
 */
export function statusVariant(
	check: HostCheck,
): "success" | "warning" | "destructive" | "outline" {
	if (check.status === "ok") return "success";
	if (check.status === "inapplicable") return "outline";
	return check.severity === "critical" ? "destructive" : "warning";
}

/**
 * Worst-first. The host already sorts, but the console re-sorts because it also renders lists it
 * filtered itself, and an ordering that depends on which rows were dropped is a bug waiting to
 * happen.
 */
export function worstFirst(checks: HostCheck[]): HostCheck[] {
	const severityRank = { critical: 0, warning: 1, info: 2 } as const;
	const statusRank = { fail: 0, warn: 1, ok: 2, inapplicable: 3 } as const;
	return [...checks].sort((a, b) => {
		const attention = Number(!needsAttention(a)) - Number(!needsAttention(b));
		if (attention !== 0) return attention;
		const severity = severityRank[a.severity] - severityRank[b.severity];
		if (severity !== 0) return severity;
		const status = statusRank[a.status] - statusRank[b.status];
		if (status !== 0) return status;
		return a.id.localeCompare(b.id);
	});
}
