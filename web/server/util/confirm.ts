// Password re-confirmation for the routes where an authenticated session is NOT enough.
//
// The console's session cookie lives for 7 days, so on its own it must not be able to run new code
// on the host. These routes clear that bar and each re-verifies the console password HERE (only the
// BFF knows it), strips it, and never forwards it:
//
//   - POST /api/v1/update/apply            — update-and-restart the host
//   - POST /api/v1/store/install           — but only for a RAW SPEC (`accept_unverified`), which
//                                            runs an unreviewed package
//   - PUT  /api/v1/store/sources/{name}    — adds a catalog SOURCE, i.e. a new trust root
//   - PUT  /api/v1/hooks                   — a hook is a shell command the host runs on its events
//   - the library writes that carry `prep` or a privileged launch kind — same primitive, gated
//     conditionally in util/libraryConfirm.ts
//   - POST /api/v1/actions/{id}           — the host power actions (sleep/reboot/shutdown,
//                                            design/host-actions.md §7): ending the machine from
//                                            a 7-day cookie alone is exactly what this gate exists
//                                            to prevent
//   - DELETE /api/v1/profiles/{id}        — with `erase` it deletes a player's Steam and saves
//   - PUT  /api/v1/profiles/door          — moves the box's host between a user's session and a
//                                            system service, and its identity and pairings with it
//   - the PAIRING routes — arming a window, approving a knock, submitting a GameStream PIN. A
//     paired device injects keyboard and mouse on the host desktop, so admitting one IS code
//     execution, and it was the shortest path past this gate (security-review 2026-08-25).
//
// A catalog install from an already-trusted source is deliberately NOT gated: the operator made
// that trust decision when they added the source, and re-prompting on every install would train
// them to type the password without reading. The gate belongs at the trust boundary, not past it.
// Same reason unpairing and denying are not gated — both only ever narrow what the host trusts.
//
// Wrong attempts share the login throttle's per-peer budget, so none of these can be used as a
// password oracle, and a lockout covers all of them at once.
import { createError, type H3Event, setResponseHeader } from "h3";
import { authConfigured, peerAddress, verifyUiPassword } from "./auth";
import {
	recordLoginFailure,
	recordLoginSuccess,
	throttleRetryAfterMs,
} from "./loginThrottle";

/**
 * Verify the re-entered console password, or throw the right HTTP error (503 unconfigured,
 * 429 throttled, 401 wrong). Resolves with nothing on success — the caller proceeds.
 *
 * Async because the compare is an argon2id verify against the stored hash. Every caller must
 * `await` it: a dropped promise would let the gated route run before the password is checked.
 */
export async function confirmPassword(
	event: H3Event,
	password: unknown,
): Promise<void> {
	if (!authConfigured()) {
		throw createError({
			statusCode: 503,
			statusMessage: "auth not configured",
		});
	}
	const ip = peerAddress(event);
	const wait = throttleRetryAfterMs(ip);
	if (wait > 0) {
		setResponseHeader(event, "Retry-After", Math.ceil(wait / 1000));
		throw createError({
			statusCode: 429,
			statusMessage: "too many attempts — try again shortly",
		});
	}
	if (!(await verifyUiPassword(String(password ?? "")))) {
		recordLoginFailure(ip);
		throw createError({
			statusCode: 401,
			statusMessage: "password confirmation failed",
		});
	}
	recordLoginSuccess(ip);
}
