// PUT /api/v1/profiles/{id}/lock/pin — a PIN opens a locked profile to any paired device that
// knows it, so setting or changing one asks for the console password (util/confirm.ts). The
// password is verified here, stripped and never forwarded; only the PIN reaches the host.
//
// Wins over the `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, getRouterParam, readBody } from "h3";
import type { ProfilePin } from "../../../../../../../src/api/gen/model";
import { confirmPassword } from "../../../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../../../util/forward";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	const body = await readBody<ProfilePin & { password?: string }>(event);
	await confirmPassword(event, body?.password);
	const upstream = { pin: body?.pin } satisfies AllFields<ProfilePin>;
	return forwardJson(
		event,
		`/api/v1/profiles/${encodeURIComponent(id)}/lock/pin`,
		"PUT",
		upstream,
	);
});
