// POST /api/v1/profiles/{id}/lock — locking only narrows who opens a profile, but a call that
// carries `pin` also sets the PIN, which opens it to any paired device that knows it: that call
// asks for the console password (util/confirm.ts). The password is verified here, stripped and
// never forwarded. Wins over the `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, getRouterParam } from "h3";
import type { LockProfile } from "../../../../../../src/api/gen/model";
import { confirmWhen, type When } from "../../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../../util/forward";

export const when: When = ({ body }) => body.pin !== undefined;

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	const body = await confirmWhen<LockProfile>(event, when);
	const upstream = {
		devices: body.devices,
		pin: body.pin,
	} satisfies AllFields<LockProfile>;
	return forwardJson(
		event,
		`/api/v1/profiles/${encodeURIComponent(id)}/lock`,
		"POST",
		upstream,
	);
});
