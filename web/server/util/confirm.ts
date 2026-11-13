// Password re-confirmation for the routes where an authenticated session is NOT enough.
//
// The console's session cookie lives for 7 days, so on its own it must not be able to run new code
// on the host, end the machine, move its identity, or admit a device (a paired device injects
// input, so pairing IS code execution). Each such route re-verifies the password HERE (only the
// BFF knows it), strips it, and never forwards it. `GATES` in util/gates.ts is the list;
// gates.test.ts asserts it names every route that imports this module or util/libraryConfirm.ts,
// each gated through a seat too or refused by the seat's host.
//
// A catalog install from an already-trusted source is deliberately NOT gated: the operator made
// that trust decision when they added the source, and re-prompting on every install would train
// them to type the password without reading. The gate belongs at the trust boundary, not past it.
// Same reason unpairing and denying are not gated — both only ever narrow what the host trusts.
//
// Wrong attempts share the login throttle's per-peer budget, so none of these can be used as a
// password oracle, and a lockout covers all of them at once.
import {
	createError,
	getRouterParams,
	type H3Event,
	readBody,
	setResponseHeader,
} from "h3";
import { authConfigured, peerAddress, verifyUiPassword } from "./auth";
import { recordLoginSuccess, takeAttempt } from "./loginThrottle";

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
	const wait = takeAttempt(ip);
	if (wait > 0) {
		setResponseHeader(event, "Retry-After", Math.ceil(wait / 1000));
		throw createError({
			statusCode: 429,
			statusMessage: "too many attempts — try again shortly",
		});
	}
	if (!(await verifyUiPassword(String(password ?? "")))) {
		throw createError({
			statusCode: 401,
			statusMessage: "password confirmation failed",
		});
	}
	recordLoginSuccess(ip);
}

/** What a conditional gate reads: the call's JSON body and its route params. */
export interface Call {
	body: Record<string, unknown>;
	params: Record<string, string>;
}

/** Whether a call needs the password. A gated row without one always does. */
export type When = (call: Call) => boolean | Promise<boolean>;

/**
 * The call's JSON body, after `confirmPassword` when `when` holds for it. A body that is not an
 * object reads as `{}`. The caller rebuilds what it forwards from the body `when` read.
 *
 * Async because the verify is: the caller must `await` this, or a gated call runs unchecked.
 */
export async function confirmWhen<T extends object>(
	event: H3Event,
	when: When,
): Promise<Partial<T> & { password?: unknown }> {
	const raw: unknown = await readBody(event);
	const body = (
		raw && typeof raw === "object" && !Array.isArray(raw) ? raw : {}
	) as Record<string, unknown>;
	if (await when({ body, params: getRouterParams(event) }))
		await confirmPassword(event, body.password);
	return body as Partial<T>;
}
