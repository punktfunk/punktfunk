// POST /api/v1/library/custom — creating a custom entry can install a command the host later runs
// as the host user (`prep`, or a privileged launch kind), so it joins hooks/update-apply/raw-install
// behind the console password when — and only when — the payload carries one of those fields.
// See util/libraryConfirm.ts for the reasoning; 2026-08-05 review M-6.
//
// Wins over the `/api/**` catch-all by h3 route specificity.
import { defineEventHandler, readBody } from "h3";
import { forwardJson } from "../../../../util/forward";
import { confirmIfCommandExecution } from "../../../../util/libraryConfirm";

export default defineEventHandler(async (event) => {
	const body = await readBody<Record<string, unknown>>(event);
	await confirmIfCommandExecution(event, body, body?.password);
	// Strip only the password: CustomInput flattens GameMeta, so a field-list rebuild would wipe
	// metadata the host adds. The gate above reads `prep` and `launch` off this same body.
	const { password: _password, ...entry } = body ?? {};
	return forwardJson(event, "/api/v1/library/custom", "POST", entry);
});
