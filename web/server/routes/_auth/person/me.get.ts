// GET /_auth/person/me — who the person's console belongs to, for its frame. Each read asks their
// seat host, with their session, for their profile and the computer's name, so a session that
// ended there ends here too.
import { defineEventHandler, type H3Event } from "h3";
import {
	isTimeout,
	type PersonSession,
	SeatGone,
	seatCall,
} from "../../../util/person";
import { ended } from "../../../util/personApi";

export interface PersonMe {
	profile: string;
	name: string;
	accent: string | null;
	/** The owner's display name: they add the shared games. */
	owner: string | null;
	/** A seat on the person's own account. */
	own: boolean;
	/** The picture's URL on this console, when they have one. */
	avatar: string | null;
	/** The computer's name, as its host reports it. */
	host: string | null;
	/** Unix seconds. */
	expires_at: number;
}

const ACCENT = /^#[0-9a-f]{6}$/i;

/** `path` on the seat host as JSON, `undefined` when it is slow or says no. Throws a `Response` when over. */
async function seatJson(
	event: H3Event,
	session: PersonSession,
	path: string,
): Promise<Record<string, unknown> | undefined> {
	let res: Response;
	try {
		res = await seatCall(session, path, {
			signal: AbortSignal.timeout(5_000),
		});
	} catch (e) {
		if (e instanceof SeatGone) throw ended(event, "person_seat_stopped");
		if (isTimeout(e)) return undefined;
		throw e;
	}
	if (res.status === 401) throw ended(event, "person_session_ended");
	if (!res.ok) return undefined;
	const body = (await res.json().catch(() => null)) as unknown;
	return body && typeof body === "object"
		? (body as Record<string, unknown>)
		: undefined;
}

export default defineEventHandler(
	async (event): Promise<PersonMe | Response> => {
		const session = event.context.person as PersonSession | undefined;
		if (!session) return ended(event, "person_session_ended");
		let me: Record<string, unknown> | undefined;
		let info: Record<string, unknown> | undefined;
		try {
			[me, info] = await Promise.all([
				seatJson(event, session, "/api/v1/auth/person/profile"),
				seatJson(event, session, "/api/v1/host"),
			]);
		} catch (answer) {
			if (answer instanceof Response) return answer;
			throw answer;
		}
		// The seat host answers this lane's own profile alone.
		if (me && me.id !== session.profile)
			return ended(event, "person_session_ended");
		const accent = me?.accent;
		return {
			profile: session.profile,
			name: typeof me?.display_name === "string" ? me.display_name : "",
			accent: typeof accent === "string" && ACCENT.test(accent) ? accent : null,
			owner: session.owner,
			own: session.own,
			avatar: me?.has_avatar === true ? "/_auth/person/picture" : null,
			host: typeof info?.hostname === "string" ? info.hostname : null,
			expires_at: Math.floor(session.expiresAt / 1000),
		};
	},
);
