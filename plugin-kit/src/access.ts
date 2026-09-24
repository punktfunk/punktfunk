import * as os from "node:os";
import * as path from "node:path";
import { Effect } from "effect";
import { HostClient } from "./host-client.js";
import { dirAccess } from "./library/parsers/fs.js";

export interface AccessRequestPath {
	readonly path: string;
	readonly write?: boolean;
}

export interface AccessRequestOutcome {
	readonly path: string;
	readonly outcome: string;
}

/** Each kind of failed request is logged once per process, not on every poll. */
const warned = new Set<string>();

const statusOf = (value: unknown): number | undefined => {
	if (typeof value !== "object" || value === null) return undefined;
	const record = value as Record<string, unknown>;
	if (typeof record.status === "number") return record.status;
	if (typeof record.statusCode === "number") return record.statusCode;
	return statusOf(record.cause);
};

const why = (status: number | undefined, message: string): string => {
	if (status === 404)
		return "the host is too old for folder access requests — update it to let plugins ask for launcher folders";
	if (status === 403)
		return "this plugin has no identity of its own on this host, so it can't ask for folders — allow one with `punktfunk-host plugins grant`";
	return `folder access request failed — ${message}`;
};

/** `~/x` → `<home>/x`, as manifest paths expand; the host refuses anything relative. */
const expand = (p: string): string =>
	p.startsWith("~/") ? path.join(os.homedir(), p.slice(2)) : p;

/**
 * Ask the host to put these folders before the operator; this never grants access itself. It
 * never fails either: a host that can't take the request is logged once, and the scan goes on
 * with what it can reach.
 */
export const requestAccess = (
	paths: ReadonlyArray<string | AccessRequestPath>,
	reason?: string,
): Effect.Effect<AccessRequestOutcome[], never, HostClient> =>
	Effect.gen(function* () {
		const host = yield* HostClient;
		const body = {
			paths: paths.map((entry) =>
				typeof entry === "string"
					? { path: expand(entry) }
					: { ...entry, path: expand(entry.path) },
			),
			...(reason ? { reason } : {}),
		};
		return yield* host.request("POST", "/plugin-access/requests", body).pipe(
			Effect.map((value) =>
				(Array.isArray(value) ? value : []).filter(
					(row): row is AccessRequestOutcome =>
						typeof row === "object" &&
						row !== null &&
						typeof (row as { path?: unknown }).path === "string" &&
						typeof (row as { outcome?: unknown }).outcome === "string",
				),
			),
			Effect.catch((error) => {
				const text = why(statusOf(error.cause), error.message);
				if (warned.has(text)) return Effect.succeed([]);
				warned.add(text);
				return Effect.logWarning(text).pipe(Effect.as([]));
			}),
		);
	});

/** Folders unusable now. Inside a sandbox, missing may mean merely unbound, so ask the host. */
export const unreachable = (dirs: ReadonlyArray<string>): string[] => {
	const sandboxed = !!process.env.PUNKTFUNK_MGMT_UNIX;
	return [...new Set(dirs.map(expand))].filter((dir) => {
		const access = dirAccess(dir);
		return access === "denied" || (sandboxed && access === "missing");
	});
};
