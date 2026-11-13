// PUT /api/v1/library/own — the box's door hands a seat's own titles to the owner's host, with
// that host's token. The console never does, so it answers here and never relays the call with
// the admin bearer. Wins over the `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, setResponseStatus } from "h3";

export default defineEventHandler((event) => {
	setResponseStatus(event, 403);
	return {
		error:
			"Only the box's door stores a seat's own titles. The console has none to send.",
	};
});
