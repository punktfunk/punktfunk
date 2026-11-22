// POST /api/v1/game/handoff/{token} — `punktfunk-host play` reports the game it started, with the
// host's own token. The console never does, so it answers here and never relays the call with
// the admin bearer. Wins over the `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, setResponseStatus } from "h3";

export default defineEventHandler((event) => {
	setResponseStatus(event, 403);
	return {
		error:
			"Only a Steam entry reports on its launch. The console started none.",
	};
});
