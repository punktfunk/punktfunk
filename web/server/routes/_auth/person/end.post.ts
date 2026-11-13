// POST /_auth/person/end — the person signs out: their session ends here and on their seat host.
// An operator session in the same browser stays as it was.
import { defineEventHandler } from "h3";
import { endSession } from "../../../util/person";

export default defineEventHandler(async (event) => {
	await endSession(event);
	return { ok: true };
});
