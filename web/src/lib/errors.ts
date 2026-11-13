import { ApiError } from "@/api/fetcher";
import { apiText } from "@/lib/host-messages";

/** The body of a failed host call: the `ApiError` envelope, or the BFF's own thrown refusal. */
type Body = {
	error?: unknown;
	code?: unknown;
	params?: Record<string, string> | null;
	statusMessage?: string;
};

/** The host's `code` of a failed call: what the console tests, never the sentence. */
export function apiErrorCode(err: unknown): string | undefined {
	if (!(err instanceof ApiError)) return undefined;
	const code = (err.data as Body | undefined)?.code;
	return typeof code === "string" ? code : undefined;
}

/**
 * The server's message from a thrown `ApiError`, for inline display. The host's `code` and
 * `params` come first, worded as `apierr_<code>` in the console's language. Without a message for
 * the code, the host's `{ error }` string stands, then the `statusMessage` of a refusal the BFF
 * throws through h3 (whose body carries `error: true`), then the HTTP status text, then whatever
 * was thrown.
 */
export function apiErrorMessage(err: unknown): string | undefined {
	if (err instanceof ApiError) {
		const data = err.data as Body | undefined;
		if (typeof data?.error === "string") {
			return apiText(apiErrorCode(err), data.params, data.error);
		}
		return data?.statusMessage || err.message;
	}
	return err ? String(err) : undefined;
}
