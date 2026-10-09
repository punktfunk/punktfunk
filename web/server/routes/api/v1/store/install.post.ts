// POST /api/v1/store/install — wins over the `/api/**` catch-all (h3 route specificity), so the
// raw-spec branch can never reach the host without a password.
//
// Two shapes arrive here:
//   { source, id }                    — a curated catalog entry. Forwarded as-is: the operator
//                                       already made the trust decision when they added the source.
//   { spec, accept_unverified: true } — an unreviewed package, no catalog, no pinning. This is
//                                       arbitrary code execution on the host, so it is gated on the
//                                       console password exactly like update/apply (util/confirm.ts).
import { defineEventHandler, readBody } from "h3";
import type { InstallRequest } from "../../../../../src/api/gen/model";
import { confirmPassword } from "../../../../util/confirm";
import { type AllFields, forwardJson } from "../../../../util/forward";

export default defineEventHandler(async (event) => {
	const body = await readBody<InstallRequest & { password?: string }>(event);
	const rawSpec = body?.accept_unverified === true;
	if (rawSpec) await confirmPassword(event, body?.password);
	// The password stops here — rebuild the upstream body from the contract's fields so it cannot
	// leak through, and so an unexpected extra field can't ride along. The other shape's keys stay
	// undefined, which `JSON.stringify` drops.
	const upstream = {
		spec: rawSpec ? String(body?.spec ?? "") : undefined,
		accept_unverified: rawSpec ? true : undefined,
		source: rawSpec ? undefined : String(body?.source ?? ""),
		id: rawSpec ? undefined : String(body?.id ?? ""),
	} satisfies AllFields<InstallRequest>;
	return forwardJson(event, "/api/v1/store/install", "POST", upstream);
});
