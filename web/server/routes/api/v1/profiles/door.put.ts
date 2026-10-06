// PUT /api/v1/profiles/door — turning Reachable without logging in on or off moves the box's host
// between the owner's session and a system service, and its files with it. That is root work the
// host starts on the operator's word, so it joins update/apply behind the console password
// (util/confirm.ts): a 7-day session cookie must not be enough to move it.
//
// The password is verified here, stripped and never forwarded. Wins over the `/api/**` catch-all
// and the `[id]` route by h3 route specificity.
import { defineEventHandler, readBody } from "h3";
import type { DoorChange } from "../../../../../src/api/gen/model";
import { confirmPassword } from "../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../util/forward";

export default defineEventHandler(async (event) => {
	const body = await readBody<DoorChange & { password?: string }>(event);
	await confirmPassword(event, body?.password);
	const upstream = { on: body?.on === true } satisfies AllFields<DoorChange>;
	return forwardJson(event, "/api/v1/profiles/door", "PUT", upstream);
});
