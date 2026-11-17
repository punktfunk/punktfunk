// Kit-level error taxonomy. `Data.TaggedError` (matching the SDK's idiom in
// sdk/src/client.ts) — these never cross HTTP; a plugin's UI-API contract defines its own
// Schema-based errors with status annotations.
import { Data } from "effect";

/**
 * A management-API call through the pf facade failed.
 *
 * The `message` getter is load-bearing, not decoration. `Data.TaggedError`'s default string form is
 * the bare tag, so without it a host that refused a reconcile with a clear 400 shows in the plugin
 * log as `HostRequestError` alone, with the method, the path and the host's own explanation
 * discarded, and a validation refusal looks the same as the host being down.
 */
export class HostRequestError extends Data.TaggedError("HostRequestError")<{
	readonly method: string;
	readonly path: string;
	readonly cause: unknown;
}> {
	override get message(): string {
		return `${this.method} ${this.path} failed: ${describeCause(this.cause)}`;
	}
}

/**
 * Render whatever `pf.request` rejected with into one line.
 *
 * An `Error` stringifies usefully already; a plain object (the host's `{error: "…"}` body)
 * stringifies to `[object Object]`. JSON is the fallback so a body-shaped cause survives, and a
 * cycle or a BigInt degrades to `String(cause)` rather than throwing inside error formatting.
 */
const describeCause = (cause: unknown): string => {
	if (cause instanceof Error) return cause.message;
	if (typeof cause === "object" && cause !== null) {
		try {
			return JSON.stringify(cause);
		} catch {
			return String(cause);
		}
	}
	return String(cause);
};

/** config.json exists but does not parse/decode. */
export class ConfigParseError extends Data.TaggedError("ConfigParseError")<{
	readonly path: string;
	readonly issue: string;
}> {}

/**
 * config.json is group/world-writable (POSIX). This file controls commands run as the
 * host user, so the kit refuses it — the same sshd rule the runner applies to unit files.
 */
export class ConfigPermissionError extends Data.TaggedError(
	"ConfigPermissionError",
)<{
	readonly path: string;
	readonly mode: number;
}> {
	override get message(): string {
		return `refusing ${this.path}: it is group/world-writable (chmod go-w it first) — this file controls commands run as the host user`;
	}
}

/** Persisting config/state failed. */
export class ConfigWriteError extends Data.TaggedError("ConfigWriteError")<{
	readonly path: string;
	readonly cause: unknown;
}> {}

/** The plugin UI server could not be started/registered. */
export class UiServeError extends Data.TaggedError("UiServeError")<{
	readonly cause: unknown;
}> {}

/** A sync pass failed (compute or apply). */
export class SyncError extends Data.TaggedError("SyncError")<{
	readonly reason: string;
	readonly cause: unknown;
}> {
	/**
	 * Same load-bearing getter as {@link HostRequestError}, and for the same reason one step further
	 * out: without it `String(e)` is the bare tag `SyncError`, so a plugin that renders its sync
	 * failure into an API error or a toast shows the operator a word instead of the refusal.
	 * `describeCause` unwraps a nested `HostRequestError` through its own message getter, so the
	 * host's sentence survives the whole way to the surface.
	 */
	override get message(): string {
		return `sync (${this.reason}) failed: ${describeCause(this.cause)}`;
	}
}
