// POST /api/v1/game/holds/{token}/release — the box's door releases a seat's launch hold once
// the owner's plugins have had their say, with that seat host's token. The console never
// does, so it answers here and never relays the call with the admin bearer. Wins over the
// `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, setResponseStatus } from "h3";

export default defineEventHandler((event) => {
	setResponseStatus(event, 403);
	return {
		error:
			"Only the box's door releases a launch hold. The console holds no launch.",
	};
});
