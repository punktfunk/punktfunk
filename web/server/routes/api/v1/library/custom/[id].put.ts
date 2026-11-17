// PUT /api/v1/library/custom/{id} — same primitive and same gate as the create route: an UPDATE
// can install `prep` / a privileged launch kind just as well as a create can, and a gate that only
// covered create would be one PUT away from pointless. See util/libraryConfirm.ts.
import { defineEventHandler, getRouterParam, readBody } from "h3";
import { forwardJson } from "../../../../../util/forward";
import { confirmIfCommandExecution } from "../../../../../util/libraryConfirm";

export default defineEventHandler(async (event) => {
	const id = getRouterParam(event, "id") ?? "";
	const body = await readBody<Record<string, unknown>>(event);
	await confirmIfCommandExecution(event, body, body?.password);
	const { password: _password, ...entry } = body ?? {};
	return forwardJson(
		event,
		`/api/v1/library/custom/${encodeURIComponent(id)}`,
		"PUT",
		entry,
	);
});
