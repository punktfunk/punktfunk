// GET /_auth/person/picture — the person's own picture, from their seat host with their session.
// The browser revalidates by its tag, and the seat host answers 304 while it is the same.
import { defineEventHandler, getRequestHeader } from "h3";
import {
	isTimeout,
	type PersonSession,
	SeatGone,
	seatCall,
} from "../../../util/person";
import { ended } from "../../../util/personApi";

const PICTURE = /^image\/(png|jpeg|webp)$/;

export default defineEventHandler(async (event) => {
	const session = event.context.person as PersonSession | undefined;
	if (!session) return ended(event, "person_session_ended");
	const headers = new Headers();
	const tag = getRequestHeader(event, "if-none-match");
	if (tag) headers.set("if-none-match", tag);
	let res: Response;
	try {
		res = await seatCall(session, "/api/v1/auth/person/profile/avatar", {
			headers,
		});
	} catch (e) {
		if (e instanceof SeatGone) return ended(event, "person_seat_stopped");
		if (isTimeout(e)) return new Response(null, { status: 504 });
		throw e;
	}
	if (res.status === 401) return ended(event, "person_session_ended");
	const type = res.headers.get("content-type") ?? "";
	const ok = res.status === 304 || (res.ok && PICTURE.test(type));
	if (!ok) return new Response(null, { status: 404 });
	const out = new Headers({ "cache-control": "private, no-cache" });
	const etag = res.headers.get("etag");
	if (etag) out.set("etag", etag);
	if (res.status !== 304) out.set("content-type", type);
	return new Response(res.status === 304 ? null : res.body, {
		status: res.status,
		headers: out,
	});
});
