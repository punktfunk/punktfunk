// POST /_auth/login {password} — verify the shared password against its stored argon2id hash,
// then seal an authenticated session cookie. Public (allowlisted in the gate) so an
// unauthenticated user can actually log in. Brute force is bounded by an in-memory per-IP
// throttle (loginThrottle); the argon2 verify costs ~50 ms, which that throttle already covers.
import {
	createError,
	defineEventHandler,
	readBody,
	setResponseHeader,
	useSession,
} from "h3";
import {
	authConfigured,
	peerAddress,
	type SessionData,
	sessionConfig,
	sessionEpoch,
	verifyUiPassword,
} from "../../util/auth";
import { recordLoginSuccess, takeAttempt } from "../../util/loginThrottle";
import { endSession } from "../../util/person";

export default defineEventHandler(async (event) => {
	if (!authConfigured()) {
		throw createError({
			statusCode: 503,
			statusMessage: "auth not configured",
		});
	}
	// The socket peer address — deliberately NOT trusting X-Forwarded-For (spoofable unless we sit
	// behind a known proxy, which the packaged console does not). See `peerAddress`: under the Bun
	// entry this is the real peer; the shared "unknown" bucket is only a last-resort fallback.
	const ip = peerAddress(event);

	// Counted before the first await, so a burst of parallel guesses meets the lockout it arms.
	const wait = takeAttempt(ip);
	if (wait > 0) {
		setResponseHeader(event, "Retry-After", Math.ceil(wait / 1000));
		throw createError({
			statusCode: 429,
			statusMessage: "too many attempts — try again shortly",
		});
	}

	const body = await readBody<{ password?: string }>(event);
	const password = String(body?.password ?? "");
	if (!(await verifyUiPassword(password))) {
		throw createError({ statusCode: 401, statusMessage: "invalid password" });
	}
	recordLoginSuccess(ip);
	const session = await useSession<SessionData>(event, sessionConfig());
	await session.update({ authenticated: true, epoch: sessionEpoch() });
	// The operator signed in: a person's console in this browser ends rather than outranking them.
	await endSession(event);
	return { ok: true };
});
