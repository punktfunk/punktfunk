// POST /_auth/logout — clear the session cookie AND revoke every session issued so far.
//
// Clearing alone only deletes the browser's copy: the cookie is stateless, so a captured value
// would stay valid for its whole 7-day TTL and "log out" would log nothing out. Bumping the epoch
// means the gate rejects every cookie sealed before now. Single-user console, so "log out" and
// "sign out everywhere" are the same action — which is the safer of the two to make the default.
//
// The global revocation is the part that needs authorizing. This route lives under `/_auth/`, which
// `isPublicPath` treats as public (the login form posts here), and the CSRF guard lets through a
// request with neither `Sec-Fetch-Site` nor `Origin` — so an unauthenticated LAN peer with `curl`
// could bump the epoch on a loop and keep the operator permanently signed out of their own
// console. Revoking is gated on holding a currently-valid session; clearing the CALLER's own
// cookie stays unconditional, because that affects nobody else and keeps a stale session's "log
// out" click behaving exactly as the user expects.
import { defineEventHandler, useSession } from "h3";
import {
	revokeAllSessions,
	type SessionData,
	sessionConfig,
	sessionEpoch,
} from "../../util/auth";

export default defineEventHandler(async (event) => {
	const session = await useSession<SessionData>(event, sessionConfig());
	// Read the state BEFORE clearing — `clear()` wipes what we need to authorize the revocation.
	const authenticated =
		session.data.authenticated === true &&
		session.data.epoch === sessionEpoch();
	await session.clear();
	if (authenticated) revokeAllSessions();
	return { ok: true };
});
