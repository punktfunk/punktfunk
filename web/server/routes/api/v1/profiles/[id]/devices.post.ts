// POST /api/v1/profiles/{id}/devices — putting a device on a locked profile's list lets it open
// that profile, so it asks for the console password (util/confirm.ts). The password is verified
// here, stripped and never forwarded.
//
// Wins over the `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, getRouterParam, readBody } from "h3";
import type { ProfileDeviceAdd } from "../../../../../../src/api/gen/model";
import { confirmPassword } from "../../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../../util/forward";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	const body = await readBody<ProfileDeviceAdd & { password?: string }>(event);
	await confirmPassword(event, body?.password);
	const upstream = {
		fingerprint: body?.fingerprint,
	} satisfies AllFields<ProfileDeviceAdd>;
	return forwardJson(
		event,
		`/api/v1/profiles/${encodeURIComponent(id)}/devices`,
		"POST",
		upstream,
	);
});
