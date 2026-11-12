// The calls a console session may make only with the password, where the answer depends on the
// body. The console's server (server/util/gates.ts) and the pages that ask both read these, so a
// page asks exactly when the server would refuse it without the password.

/** Carries the console password on a plugin form's save, whose body goes to the plugin as typed. */
export const PASSWORD_HEADER = "x-punktfunk-password";

/** Host settings that open a plane to devices or weaken one. Any change to them asks. */
export const PLANE_SETTINGS = [
	"gamestream",
	"webtransport",
	"webtransport_origins",
	"gamestream_encrypt",
	"clipboard",
] as const;

/** Whether a host-settings patch changes a plane setting. */
export const touchesPlane = (patch: object): boolean =>
	PLANE_SETTINGS.some((id) => id in patch);

/** `GRANT_ALL` in `punktfunk-core` `quic/access.rs`; an absent stored mask means it. */
const GRANT_ALL = 0x1ff;

/** A paired device's access as the host stores it. */
export interface StoredAccess {
	grants?: number | null;
	expires_unix?: number | null;
	until_disconnect?: boolean | null;
}

/** The fields of `PATCH native/clients/{fingerprint}`, as sent. */
export interface AccessPatch {
	grants?: unknown;
	expires_in_secs?: unknown;
	clear_expiry?: unknown;
	until_disconnect?: unknown;
}

/**
 * Whether `patch` gives a device a grant it lacks, or keeps it longer than `stored` does. An
 * unknown device and a value of the wrong type count as widening: the host refuses those itself,
 * after the password.
 */
export function widensAccess(
	stored: StoredAccess | undefined,
	patch: AccessPatch,
	nowUnix: number,
): boolean {
	if (!stored) return true;
	const held = (stored.grants ?? GRANT_ALL) & GRANT_ALL;
	const deadline = stored.expires_unix ?? null;
	const { grants, expires_in_secs: secs, clear_expiry: clear } = patch;
	const session = patch.until_disconnect;
	const int = (v: unknown): v is number => Number.isInteger(v);
	if (grants != null && (!int(grants) || (grants & ~held) !== 0)) return true;
	if (clear != null && clear !== false && (clear !== true || deadline !== null))
		return true;
	if (
		secs != null &&
		(!int(secs) || (secs > 0 && deadline !== null && nowUnix + secs > deadline))
	)
		return true;
	return (
		session != null &&
		session !== true &&
		(session !== false || stored.until_disconnect === true)
	);
}
