// DELETE /api/v1/profiles/{id}/lock — unlocking a profile lets every paired device open it again,
// the owner's desktop included, so it asks for the console password (util/confirm.ts): a 7-day
// session cookie alone must not undo a lock. The password is verified here, never forwarded.
//
// Wins over the `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, getRouterParam, readBody } from "h3";
import { confirmPassword } from "../../../../../util/confirm";
import { forwardJson } from "../../../../../util/forward";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	const body = await readBody<{ password?: string }>(event);
	await confirmPassword(event, body?.password);
	return forwardJson(
		event,
		`/api/v1/profiles/${encodeURIComponent(id)}/lock`,
		"DELETE",
	);
});
