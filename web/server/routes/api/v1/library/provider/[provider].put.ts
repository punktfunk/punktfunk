// PUT /api/v1/library/provider/{provider} — the reconcile route a provider plugin uses to publish
// its whole set of entries at once. It carries the SAME primitive the create/update routes gate:
// an entry in the payload can bring `prep` or a privileged launch kind, both run as the host
// user. The host defers the "is this the operator's authority?" question to the console
// (mgmt/library.rs), and the console must actually ask it — otherwise a bare session cookie plants
// a persistent command through this lane while `custom.post.ts`/`custom/[id].put.ts` are gated.
// See util/libraryConfirm.ts.
//
// Wins over the `/api/**` catch-all by h3 route specificity — the catch-all injects the admin
// bearer unconditionally, so without this file the host-side check is inert for this caller.
import {
	defineEventHandler,
	getRequestURL,
	getRouterParam,
	readBody,
} from "h3";
import { forwardJson } from "../../../../../util/forward";
import { confirmIfCommandExecution } from "../../../../../util/libraryConfirm";

// The host takes a bare array of entries. Accept that, and also a `{ password, entries }` wrapper
// so the console can carry the confirmation the gate needs; either way only the array is forwarded.
type Entry = Record<string, unknown>;
interface Wrapper {
	password?: unknown;
	entries?: Entry[];
}

export default defineEventHandler(async (event) => {
	const provider = getRouterParam(event, "provider") ?? "";
	const body = await readBody<Entry[] | Wrapper>(event);
	const entries = Array.isArray(body) ? body : (body?.entries ?? []);
	const password = Array.isArray(body) ? undefined : body?.password;
	// `confirmIfCommandExecution` already iterates arrays and only prompts when an entry actually
	// carries a shell field — an ordinary catalog reconcile forwards untouched.
	await confirmIfCommandExecution(event, entries, password);
	// Preserve `?store=` (the provider routes are store-qualified upstream).
	const { search } = getRequestURL(event);
	return forwardJson(
		event,
		`/api/v1/library/provider/${encodeURIComponent(provider)}${search}`,
		"PUT",
		entries,
	);
});
