// POST /api/v1/update/apply — a proxied route with an extra gate: the console password must be
// re-entered per apply (design host-update-from-web-console.md §4.3). A 7-day session cookie alone
// must not be able to update-and-restart the host; the password is verified in `confirmPassword`
// (only the BFF knows it), stripped, and never forwarded. Wrong attempts share the login throttle's
// per-peer budget, so apply can't be used as a password oracle.
//
// This specific file wins over the `[...]` catch-all (h3 route specificity) — verified in the
// U1 gate; everything else about proxying (bearer injection, loopback TLS scoping, 401→502)
// lives in util/forward.ts and mirrors ../../[...].ts.
import { defineEventHandler, readBody } from "h3";
import type { ApplyRequest } from "../../../../../src/api/gen/model";
import { confirmPassword } from "../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../util/forward";

export default defineEventHandler(async (event) => {
	const body = await readBody<ApplyRequest & { password?: string }>(event);
	await confirmPassword(event, body?.password);
	// The password stops here — the host only ever sees the contract's fields.
	const upstream = {
		force: body?.force === true,
	} satisfies AllFields<ApplyRequest>;
	return forwardJson(event, "/api/v1/update/apply", "POST", upstream);
});
