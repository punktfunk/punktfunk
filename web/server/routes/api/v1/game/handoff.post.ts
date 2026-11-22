// POST /api/v1/game/handoff — `punktfunk-host play` asks for a launch from inside the Steam entry
// a title was synced as, with the host's own token. The console never does, so it answers here
// and never relays the call with the admin bearer. Wins over the `/api/**` catch-all by h3
// route specificity.
import { defineEventHandler, setResponseStatus } from "h3";

export default defineEventHandler((event) => {
	setResponseStatus(event, 403);
	return {
		error:
			"Only a Steam entry hands a launch to the host. The console starts no game in Steam.",
	};
});
