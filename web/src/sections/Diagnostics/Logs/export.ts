import type { ClientLogMeta } from "@/api/gen/model/clientLogMeta";
import type { HostCheck } from "@/api/gen/model/hostCheck";
import { checkTitle, statusLabel, worstFirst } from "@/lib/diagnostics";
import type { Row } from "./rows";

/**
 * How the log entries can leave the page. Probed at runtime rather than assumed: `share` is Web
 * Share level 2 (mobile Safari/Chrome), `copy` is every secure-context desktop browser, and `null`
 * is a page served over plain HTTP to a browser without Web Share — there the clipboard API is
 * absent too, so the button is left out instead of offered as a no-op.
 */
export type ShareMode = "share" | "copy";

export type ShareOutcome = "shared" | "copied" | "cancelled" | "failed";

const MIME = "text/plain";

/**
 * One line per row, in the on-screen column order but with the full date and UTC offset — a bare
 * wall-clock time is ambiguous the moment the file leaves the browser, and bug reports span days.
 * A device row keeps its origin tag: in a merged export, "which machine said this" is the first
 * thing a reader needs and the last thing they can reconstruct.
 */
export const logsToText = (rows: Row[]): string =>
	rows
		.map(
			(r) =>
				`${stamp(r.ts)} ${r.level.padEnd(5)} ${r.device ? `[${r.device}] ` : ""}${r.target} ${r.msg}`,
		)
		.join("\n");

/** `punktfunk-logs-20260730-142231.log` — sorts chronologically in a downloads folder. */
export const logFilename = (now: Date): string =>
	`punktfunk-logs-${p(now.getFullYear(), 4)}${p(now.getMonth() + 1)}${p(now.getDate())}-${p(now.getHours())}${p(now.getMinutes())}${p(now.getSeconds())}.log`;

/** `punktfunk-diagnostics-20260730-142231.txt` — the combined export's sibling name. */
export const diagnosticsFilename = (now: Date): string =>
	`punktfunk-diagnostics-${p(now.getFullYear(), 4)}${p(now.getMonth() + 1)}${p(now.getDate())}-${p(now.getHours())}${p(now.getMinutes())}${p(now.getSeconds())}.txt`;

const banner = (title: string): string =>
	`\n${"=".repeat(72)}\n== ${title}\n${"=".repeat(72)}\n`;

/**
 * Everything this page knows, as one file: the host's health checks, the host + plugin log, and
 * every uploaded device bundle verbatim.
 *
 * **Plain text, assembled in the browser** — two deliberate calls.
 *
 * Text rather than an archive because the artifact's job is to be pasted into a bug report or
 * scrolled by the person who asked for it; a zip makes both a two-step operation and buys
 * compression nobody was short of.
 *
 * In the browser rather than from a host endpoint because the console holds *more* host log than
 * the host does: the ring is 4096 entries (`log_capture.rs`), while an open console accumulates up
 * to 5000 and keeps lines the ring has already evicted. A host-side export would quietly ship less
 * history than the page it was launched from — and it would cost a new authenticated route, a
 * regenerated OpenAPI document in both of its checked-in copies, and a row in the mgmt lane matrix.
 *
 * Device bundles go in verbatim, not re-rendered from parsed rows: a leg whose format this build
 * cannot parse must still export intact, and the raw bytes are the only version guaranteed to.
 */
export const diagnosticsText = (parts: {
	generatedAt: Date;
	checks: HostCheck[];
	checksUnavailable?: boolean;
	rows: Row[];
	bundles: { meta: ClientLogMeta; text: string }[];
}): string => {
	const { generatedAt, checks, checksUnavailable, rows, bundles } = parts;
	const out: string[] = [
		`punktfunk diagnostics export`,
		`generated: ${stamp(generatedAt.getTime())}`,
		`host log lines: ${rows.length}`,
		`device bundles: ${bundles.length}`,
	];

	out.push(banner("HEALTH CHECKS"));
	if (checksUnavailable) {
		out.push(
			"This host has no diagnostics route (it predates the checks API).",
		);
	} else if (checks.length === 0) {
		out.push("No checks reported.");
	} else {
		for (const c of worstFirst(checks)) {
			out.push(`[${statusLabel(c)}] ${checkTitle(c)} (${c.id})`);
			if (c.summary) out.push(`  summary: ${c.summary}`);
			if (c.impact) out.push(`  impact:  ${c.impact}`);
			if (c.remedy?.text) out.push(`  remedy:  ${c.remedy.text}`);
			if (c.remedy?.command) out.push(`  command: ${c.remedy.command}`);
		}
	}

	out.push(banner("HOST AND PLUGIN LOG"));
	out.push(rows.length ? logsToText(rows) : "No entries.");

	for (const { meta, text } of bundles) {
		out.push(
			banner(
				`DEVICE BUNDLE — ${meta.device_name} (${meta.fingerprint_prefix}), received ${stamp(meta.received_ms)}`,
			),
		);
		out.push(text.trimEnd());
	}

	// A device's clock is its own, and a bundle that looks minutes off from the host log is a
	// property of the machines, not of this file. Saying so at the bottom costs one line and saves
	// the reader from "correcting" a correlation that was never wrong.
	if (bundles.length > 0) {
		out.push(
			banner("NOTE"),
			"Device timestamps come from each device's own clock and may differ from the host's.",
		);
	}

	return `${out.join("\n")}\n`;
};

export const downloadText = (text: string, filename: string): void => {
	const url = URL.createObjectURL(new Blob([text], { type: MIME }));
	const a = document.createElement("a");
	a.href = url;
	a.download = filename;
	document.body.appendChild(a);
	a.click();
	a.remove();
	URL.revokeObjectURL(url);
};

export const detectShareMode = (): ShareMode | null => {
	if (typeof navigator === "undefined") return null; // server render
	// `canShare` decides on the file's type, not its bytes, so a stand-in probe file is enough.
	if (canShareFiles([logFile("probe", "punktfunk-logs.log")])) return "share";
	return typeof navigator.clipboard?.writeText === "function" ? "copy" : null;
};

/**
 * Hand the logs to the OS share sheet, falling back to the clipboard. Everything up to the
 * `navigator.share`/`writeText` call is synchronous on purpose: both APIs require the calling task
 * to still be the user's click, and an `await` in between would forfeit that on Safari.
 */
export const shareLogs = async (
	text: string,
	filename: string,
): Promise<ShareOutcome> => {
	const files = [logFile(text, filename)];
	if (canShareFiles(files)) {
		try {
			await navigator.share({ files, title: filename });
			return "shared";
		} catch (err) {
			// A dismissed share sheet is a deliberate cancel, not something to report back.
			return err instanceof DOMException && err.name === "AbortError"
				? "cancelled"
				: "failed";
		}
	}
	try {
		await navigator.clipboard.writeText(text);
		return "copied";
	} catch {
		return "failed";
	}
};

const logFile = (text: string, filename: string): File =>
	new File([text], filename, { type: MIME });

// Callers reach this from a click or from the guarded probe above, so `navigator` is always there.
const canShareFiles = (files: File[]): boolean =>
	typeof navigator.canShare === "function" && navigator.canShare({ files });

const p = (n: number, w = 2): string => String(n).padStart(w, "0");

/** Local ISO 8601 with a numeric offset, e.g. `2026-07-30T14:22:31.123+02:00`. */
const stamp = (ts: number): string => {
	const d = new Date(ts);
	const date = `${p(d.getFullYear(), 4)}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
	const time = `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
	// getTimezoneOffset() counts minutes *behind* UTC, so east of Greenwich is negative.
	const off = -d.getTimezoneOffset();
	const sign = off < 0 ? "-" : "+";
	const abs = Math.abs(off);
	return `${date}T${time}${sign}${p(Math.floor(abs / 60))}:${p(abs % 60)}`;
};
