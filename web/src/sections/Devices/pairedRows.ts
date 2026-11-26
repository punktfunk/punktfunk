import type { NativeClient } from "@/api/gen/model/nativeClient";
import type { PairedClient } from "@/api/gen/model/pairedClient";
import type { SessionRow } from "@/api/gen/model/sessionRow";
import type { StoredAccess } from "@/lib/password-gates";

/** The two pairing protocols a device can be paired over. */
export type PairedProtocol = "native" | "moonlight";

/** One paired device, normalized across the native + Moonlight lists. */
export interface PairedRow {
	protocol: PairedProtocol;
	fingerprint: string;
	/**
	 * What to show in the Name column. Native devices carry a name from pairing; a Moonlight client
	 * shows its operator-given label if it has one, and otherwise falls back to its cert subject —
	 * which is the same fixed string for every Moonlight client alive, hence [`label`].
	 */
	name: string;
	/**
	 * The operator-assigned label, Moonlight rows only — `null` when the device has never been
	 * named. Distinct from `name` because the rename dialog must open on the label alone: seeding
	 * it with the `CN=…` fallback would make every rename start by deleting boilerplate.
	 */
	label?: string | null;
	/**
	 * Access fields — native rows only, and only from hosts that have them (the console pairs
	 * against older hosts: all four stay `undefined` then, and the Access column shows "—").
	 * `access_level` is the presence sentinel — a current host always derives it for a
	 * NativeClient, so its absence means the host predates per-client access.
	 */
	accessLevel?: string | null;
	/** Grant bitmask; `null` = a pre-grants record = full control. */
	grants?: number | null;
	/** Absolute expiry (unix secs); `null` = permanent. "Expired" is our arithmetic. */
	expiresUnix?: number | null;
	/** The record goes when this device's last session does. Independent of `expiresUnix`. */
	untilDisconnect?: boolean | null;
	/** When it last opened a session (unix secs), native rows on a host that records it. */
	lastSeenUnix?: number | null;
	/** The OS the device last said it runs (`macos`, `web`), native rows only. */
	platform?: string | null;
}

/** Whether the host reported access fields for this row (⇒ the chip and editor exist). */
export const hasAccess = (r: PairedRow): boolean =>
	r.protocol === "native" &&
	(r.accessLevel != null || r.grants != null || r.expiresUnix != null);

/** The native list then the Moonlight list as one, each row tagged with its protocol. */
export function pairedRows(
	native: NativeClient[] | undefined,
	moonlight: PairedClient[] | undefined,
): PairedRow[] {
	return [
		...(native ?? []).map(
			(c): PairedRow => ({
				protocol: "native",
				fingerprint: c.fingerprint,
				name: c.name,
				accessLevel: c.access_level,
				grants: c.grants,
				expiresUnix: c.expires_unix,
				untilDisconnect: c.until_disconnect,
				lastSeenUnix: c.last_seen_unix,
				platform: c.platform,
			}),
		),
		...(moonlight ?? []).map(
			(c): PairedRow => ({
				protocol: "moonlight",
				fingerprint: c.fingerprint,
				name: c.label ?? c.subject ?? "",
				label: c.label,
			}),
		),
	];
}

/** The slice of a list query `pairedListView` reads. */
interface ListQuery<T> {
	data?: T[];
	isLoading: boolean;
	error: unknown;
}

/** What the paired list draws. */
export interface PairedListView {
	rows: PairedRow[];
	loading: boolean;
	/** Set when that list failed; it sits where the list's rows would. */
	nativeError?: unknown;
	moonlightError?: unknown;
	/** Show "no devices": no rows and no list that might hold some. */
	empty: boolean;
}

/**
 * Decides the paired list. A list that loaded keeps its rows when the other fails (a host built
 * without the native endpoint answers 503); the failing list's error sits where its rows would,
 * native first. Both failing is one outage and shows once.
 */
export function pairedListView(
	native: ListQuery<NativeClient>,
	moonlight: ListQuery<PairedClient>,
): PairedListView {
	const rows = pairedRows(native.data, moonlight.data);
	return {
		rows,
		loading: native.isLoading || moonlight.isLoading,
		nativeError: native.error || undefined,
		moonlightError: native.error ? undefined : moonlight.error || undefined,
		empty: rows.length === 0 && !native.error && !moonlight.error,
	};
}

/**
 * Which editors a row opens: the access chip, the display line (only on a host with per-device
 * policy, native rows), the rename (a Moonlight device's certificate carries no name).
 */
export function rowControls(r: PairedRow, perDevice: boolean) {
	return {
		access: hasAccess(r),
		display: perDevice && r.protocol === "native",
		rename: r.protocol === "moonlight",
	};
}

/** What the host stores for a native device, in the shape `widensAccess` compares against. */
export function storedAccess(
	rows: PairedRow[],
	fingerprint: string,
): StoredAccess | undefined {
	const row = rows.find(
		(r) => r.protocol === "native" && r.fingerprint === fingerprint,
	);
	return (
		row && {
			grants: row.grants,
			expires_unix: row.expiresUnix,
			until_disconnect: row.untilDisconnect,
		}
	);
}

/**
 * Whether the device with this certificate is streaming: a native session's `client` is its
 * fingerprint's prefix. A `local` row's is the key `desk`, which names no device.
 */
export const isStreaming = (sessions: SessionRow[], fingerprint: string) =>
	sessions.some(
		(s) =>
			s.plane !== "gamestream" &&
			s.plane !== "local" &&
			fingerprint.startsWith(s.client),
	);

/** The protocols holding a row; only those have an unpair-all endpoint worth calling. */
export function planesWithRows(rows: PairedRow[]) {
	return {
		native: rows.some((r) => r.protocol === "native"),
		moonlight: rows.some((r) => r.protocol === "moonlight"),
	};
}

/** The slice of an unpair mutation `pendingFingerprint` reads. */
interface UnpairCall {
	isPending: boolean;
	variables?: { fingerprint: string };
}

/** The fingerprint of the device whose unpair is running, so only its row disables. */
export function pendingFingerprint(...calls: UnpairCall[]): string | null {
	for (const c of calls) {
		if (c.isPending && c.variables) return c.variables.fingerprint;
	}
	return null;
}

/**
 * The rename request for a prompt's answer; `null` for a cancel. An empty answer clears the
 * label, which the host reads as unnamed.
 */
export function renameBody(
	answer: string | null,
): { label: string | null } | null {
	return answer === null ? null : { label: answer.trim() || null };
}

/** The title of a device's display sheet; a device without a name shows its fingerprint. */
export const deviceName = (
	r: Pick<PairedRow, "name" | "fingerprint">,
): string => r.name || r.fingerprint.slice(0, 12);
