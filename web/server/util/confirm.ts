// Password re-confirmation for the routes where an authenticated session is NOT enough.
//
// The console's session cookie lives for 7 days, so on its own it must not be able to run new code
// on the host, end the machine, move its identity, or admit a device (a paired device injects
// input, so pairing IS code execution). Each such route re-verifies the password HERE (only the
// BFF knows it), strips it, and never forwards it. The routes that import this module or
// util/libraryConfirm.ts are the list; `seatProxy.test.ts` asserts each one is gated through a
// seat too, or refused by the seat's host.
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
