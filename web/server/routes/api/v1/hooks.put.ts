// PUT /api/v1/hooks — writing a hook means writing a SHELL COMMAND the host will execute on its own
// events, as the host user. That is code execution by any other name, so it joins update/apply and
// raw-spec installs behind the console password (util/confirm.ts): a 7-day session cookie must not
// be enough to leave a persistent command behind on the machine.
//
// Wins over the `/api/**` catch-all by h3 route specificity. GET is not gated — reading the current
// automation is ordinary console business.
import { defineEventHandler, readBody } from "h3";
import type { HooksConfig } from "../../../../src/api/gen/model";
import { confirmPassword } from "../../../util/confirm";
import { type AllFields, forwardJson } from "../../../util/forward";

export default defineEventHandler(async (event) => {
	const body = await readBody<HooksConfig & { password?: string }>(event);
	await confirmPassword(event, body?.password);
	// Rebuild from the contract's fields, so the password cannot leak upstream.
	const upstream = {
		hooks: Array.isArray(body?.hooks) ? body.hooks : [],
	} satisfies AllFields<HooksConfig>;
	return forwardJson(event, "/api/v1/hooks", "PUT", upstream);
});
