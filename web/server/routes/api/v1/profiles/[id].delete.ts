// DELETE /api/v1/profiles/{id} — removing a profile is password-gated: with `erase` it deletes
// that player's Steam and saved games, and a 7-day session cookie alone must not be able to.
// The password is verified here, stripped, and never forwarded; only `erase` reaches the host.
//
// This specific file wins over the `[...]` catch-all (h3 route specificity), which is what
// keeps the catch-all from proxying the delete ungated.
import { defineEventHandler, getQuery, getRouterParam, readBody } from "h3";
import { confirmPassword } from "../../../../util/confirm";
import { forwardJson } from "../../../../util/forward";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	const body = await readBody<{ password?: string }>(event);
	await confirmPassword(event, body?.password);
	const erase = getQuery(event).erase === "true" ? "?erase=true" : "";
	return forwardJson(
		event,
		`/api/v1/profiles/${encodeURIComponent(id)}${erase}`,
		"DELETE",
	);
});
