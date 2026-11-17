// GET /api/v1/native/pair — the pairing status the card polls once a second while armed, with the
// **PIN stripped**.
//
// Arming is password-gated (pair/arm.post.ts). A PIN readable from this status for the life of the
// window would let a cookie-only attacker poll until the operator armed for their own device, then
// complete the ceremony first. A password cannot ride a 1 s poll, so the gate here is that the PIN
// is only ever handed back once, in the arm RESPONSE, to whoever supplied the password. Everything
// else in the status (enabled, armed, countdown, paired count) is ordinary console business and
// stays.
//
// The console keeps the armed PIN from that response; after a reload it is gone and the card falls
// back to its arm form, where re-arming mints a fresh one. DELETE (disarm) is not gated and falls
// through to the `/api/**` catch-all — closing a window only narrows what the host will accept.
import { defineEventHandler } from "h3";
import { forwardJson } from "../../../../util/forward";

export default defineEventHandler(async (event) => {
	const text = await forwardJson(event, "/api/v1/native/pair", "GET");
	try {
		const { pin: _pin, ...status } = JSON.parse(text) as { pin?: unknown };
		return JSON.stringify(status);
	} catch {
		// Not JSON (an upstream error page): relay it as-is rather than swallow it.
		return text;
	}
});
