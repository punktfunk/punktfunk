// PUT /api/v1/steam/entries — the Decky plugin stores the map of the Steam entries it made,
// through the host's own CLI and token. The console never does, so it answers here and never
// relays the call with the admin bearer. Wins over the `/api/**` catch-all by h3 route
// specificity.
import { defineEventHandler, setResponseStatus } from "h3";

export default defineEventHandler((event) => {
	setResponseStatus(event, 403);
	return {
		error:
			"Only the Decky plugin stores the Steam entries it made. The console makes none.",
	};
});
